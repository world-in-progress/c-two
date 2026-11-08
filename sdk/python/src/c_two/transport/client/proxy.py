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


class _LocalRouteClosed(Exception):
    """Native admission closed before a same-process resource was entered."""


class _FollowingLocalRoute:
    """Target shared by the views of one registered same-process connection."""

    def __init__(self, runtime, session, crm_class, name, resource, scheduler):
        self.lock = threading.Lock()
        self.acquire_gate = threading.Lock()
        self.runtime = runtime
        self.session = session
        self.crm_class = crm_class
        self.name = name
        self.local = (resource, scheduler)
        # None means the original local target; later publication is one
        # immutable local pair or one remote proxy.
        self.target = None

    def __del__(self):
        target = self.target
        if isinstance(target, CRMProxy):
            target.terminate()


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
        '_closed_event', '_close_lock', '_on_terminate',
        '_scheduler', '_local_copy_tracker', '_call_options',
        '_call_timeout', '_call_budget', '_following_local',
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
        local_copy_tracker: Any = None,
        following_local: _FollowingLocalRoute | None = None,
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
        proxy._closed_event = threading.Event()
        proxy._on_terminate = on_terminate
        proxy._scheduler = scheduler
        proxy._local_copy_tracker = local_copy_tracker
        proxy._call_options = None
        proxy._call_timeout = ...
        proxy._call_budget = None
        proxy._following_local = following_local
        return proxy

    @classmethod
    def ipc(
        cls,
        client: Any,
        name: str,
        *,
        on_terminate: Callable[[], None] | None = None,
        local_copy_tracker: Any = None,
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
        proxy._closed_event = threading.Event()
        proxy._on_terminate = on_terminate
        proxy._scheduler = None
        proxy._local_copy_tracker = local_copy_tracker
        proxy._call_options = None
        proxy._call_timeout = ...
        proxy._call_budget = None
        proxy._following_local = None
        return proxy

    @classmethod
    def http(
        cls,
        http_client: Any,
        name: str,
        *,
        on_terminate: Callable[[], None] | None = None,
        local_copy_tracker: Any = None,
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
        proxy._closed_event = threading.Event()
        proxy._on_terminate = on_terminate
        proxy._scheduler = None
        proxy._local_copy_tracker = local_copy_tracker
        proxy._call_options = None
        proxy._call_timeout = ...
        proxy._call_budget = None
        proxy._following_local = None
        return proxy

    # ------------------------------------------------------------------
    # Client interface (compatible with rpc.Client)
    # ------------------------------------------------------------------

    def __getattr__(self, name: str) -> Any:
        raise AttributeError(
            f'{type(self).__name__!r} object has no attribute {name!r}',
        )

    @property
    def supports_direct_call(self) -> bool:
        """``True`` for thread-local proxies (skip serialization)."""
        return self._mode == 'thread'

    @property
    def _closed(self) -> bool:
        return self._closed_event.is_set()

    def snapshot_followed_target(self, target) -> CRMProxy:
        """Freeze a published replacement for dispatch with this view's options."""
        if isinstance(target, tuple):
            return self._local_snapshot(*target)
        return self._remote_snapshot(target, None)

    def start_call_budget(self):
        """Start the deadline after native local admission rejects a call."""
        if self._following_local is None:
            return None
        from ..._native import NativeCallOptions
        options = self._call_options or NativeCallOptions()
        return _native_call(options.call_budget)

    def resolve_for_call(self, budget=None) -> CRMProxy:
        """Capture the current target before choosing direct or encoded dispatch."""
        state = self._following_local
        if state is None:
            return self
        if self._closed:
            raise RuntimeError('Proxy is closed')

        target = state.target
        if isinstance(target, CRMProxy):
            return self._remote_snapshot(target, budget)
        resource, scheduler = target if target is not None else state.local
        if scheduler is None or not scheduler.snapshot().closed:
            return self._local_snapshot(resource, scheduler)

        # Only callers rejected by native admission enter the acquisition gate.
        # Publication uses a separate short lock after the native work finishes.
        if budget is None:
            budget = self.start_call_budget()
        attempt = _native_call(budget.connect_attempt) if budget is not None else None
        from ..registry import _connect_lock, _connect_native_call
        with _connect_lock(state.acquire_gate, attempt, 'local_route_follow_wait'):
            current = state.target
            local = current if isinstance(current, tuple) else state.local
            if not isinstance(current, CRMProxy) and local[1].snapshot().closed:
                with _connect_lock(state.runtime._lock, attempt, 'local_session_wait'):
                    if state.runtime._runtime_session is not state.session:
                        raise RuntimeError('Runtime session was replaced during route follow')
                _connect_native_call(state.session.check_active, connect_attempt=attempt)
                try:
                    connection = state.runtime._connect(
                        state.crm_class, name=state.name, address=None, attempt=attempt,
                    )
                except LookupError as exc:
                    from ...error import ResourceClosed
                    raise ResourceClosed(
                        f"route {state.name!r} is closed",
                        details={'route': state.name},
                    ) from exc
                target = connection.client
                try:
                    with _connect_lock(state.runtime._lock, attempt, 'local_session_wait'):
                        if state.runtime._runtime_session is not state.session:
                            raise RuntimeError('Runtime session was replaced during route follow')
                    _connect_native_call(state.session.check_active, connect_attempt=attempt)
                except Exception:
                    target.terminate()
                    raise
                if target._mode == 'thread':
                    if target._scheduler is not None and target._scheduler.snapshot().closed:
                        target.terminate()
                        from ...error import ResourceClosed
                        raise ResourceClosed(
                            f"route {state.name!r} is closed",
                            details={'route': state.name},
                        )
                    with state.lock:
                        state.local = (target._crm, target._scheduler)
                        state.target = state.local
                    target.terminate()
                else:
                    with state.lock:
                        state.target = target
            current = state.target
            local = current if isinstance(current, tuple) else state.local
        if self._closed:
            raise RuntimeError('Proxy is closed')
        if not isinstance(current, CRMProxy):
            return self._local_snapshot(*local)
        return self._remote_snapshot(current, budget)

    def _local_snapshot(self, resource, scheduler) -> CRMProxy:
        target = self._snapshot()
        target._crm = resource
        target._scheduler = scheduler
        target._mode = 'thread'
        target._client = None
        target._call_budget = None
        return target

    def _remote_snapshot(self, remote, budget) -> CRMProxy:
        with self._close_lock:
            if self._closed:
                raise RuntimeError('Proxy is closed')
            client = self._client
            if client is None:
                if self._call_timeout is ...:
                    client = remote._client
                else:
                    client = _native_call(
                        remote._client.with_call_options,
                        timeout=self._call_timeout,
                    )
                    self._client = client
            target = self._snapshot()
            target._mode = client.mode
            target._client = client
            target._crm = None
            target._scheduler = None
            target._call_budget = budget
            return target

    def _snapshot(self) -> CRMProxy:
        snapshot = object.__new__(type(self))
        for name in self.__slots__:
            setattr(snapshot, name, getattr(self, name))
        snapshot._on_terminate = None
        return snapshot

    @property
    def route_name(self) -> str:
        """Routing name associated with this proxy."""
        return self._name

    @property
    def observed_path(self) -> str:
        """Core-observed transport path for this remote proxy."""
        state = self._following_local
        if state is not None and isinstance(state.target, CRMProxy):
            return state.target._client.observed_path
        if self._mode == 'thread':
            return 'thread_local'
        return self._client.observed_path

    @property
    def local_copy_tracker(self) -> Any:
        """Native local-copy tracker for this process session."""
        return self._local_copy_tracker

    def with_call_options(self, *, timeout=...) -> CRMProxy:
        """Create an independent connection view with Core-owned call options."""
        from ..._native import NativeCallOptions
        with self._close_lock:
            if self._closed:
                raise RuntimeError('Proxy is closed')
            view = object.__new__(type(self))
            for name in self.__slots__:
                setattr(view, name, getattr(self, name))
            view._close_lock = threading.Lock()
            view._closed_event = threading.Event()
            if self._mode == 'thread':
                view._call_options = NativeCallOptions(timeout=timeout)
                view._call_timeout = timeout
                view._client = None
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
        if self._call_budget is not None:
            budget = self._call_budget
            self._call_budget = None
            return _native_call(self._client.begin_call_with_budget, method_name, budget)
        return _native_call(self._client.begin_call, method_name)

    def prepare_call(self, method_name: str):
        """Project one reserved native call into the transfer interface."""
        return _PreparedClient(self.begin_call(method_name))

    def begin_stream(self, method_name: str):
        """Reserve the native stream call before encoding its input."""
        with self._close_lock:
            if self._closed:
                raise RuntimeError('Proxy is closed')
        if self._call_budget is not None:
            budget = self._call_budget
            self._call_budget = None
            return _native_call(self._client.begin_stream_with_budget, method_name, budget)
        return _native_call(self._client.begin_stream, method_name)

    def open_direct_stream(self, method_name: str, args: tuple):
        from ..._native import NativeCallOptions
        from ...crm.stream import _ResourceStream
        with self._close_lock:
            if self._closed:
                raise RuntimeError('Proxy is closed')
        try:
            _native_call((self._call_options or NativeCallOptions()).validate_thread)
        except Exception as exc:
            from ...error import UnsupportedCallMode
            if (
                isinstance(exc, UnsupportedCallMode)
                and self._following_local is not None
                and self._scheduler is not None
                and self._scheduler.snapshot().closed
            ):
                raise _LocalRouteClosed() from exc
            raise
        if self._scheduler is None:
            raise RuntimeError('same-process streams require a registered native route')
        method = getattr(self._crm, method_name)
        try:
            return _native_call(
                self._scheduler.open_stream,
                self._scheduler.method_idx(method_name),
                lambda: _ResourceStream(method, args=args, encoded=False),
            )
        except Exception as exc:
            from ...error import ResourceClosed
            if (
                isinstance(exc, ResourceClosed)
                and self._following_local is not None
                and self._scheduler.snapshot().closed
            ):
                raise _LocalRouteClosed() from exc
            raise

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
        try:
            _native_call((self._call_options or NativeCallOptions()).validate_thread)
        except Exception as exc:
            from ...error import UnsupportedCallMode
            if (
                isinstance(exc, UnsupportedCallMode)
                and self._following_local is not None
                and self._scheduler is not None
                and self._scheduler.snapshot().closed
            ):
                raise _LocalRouteClosed() from exc
            raise
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
            # Final admission can fail after acquire while Python waits to run.
            # Normalize that native error before entering resource code.
            guard.__enter__()
        except RuntimeError as exc:
            error_bytes = getattr(exc, 'error_bytes', None)
            if error_bytes is not None:
                from ...error import CCError, ResourceClosed
                cc_err = CCError.deserialize(memoryview(error_bytes))
                if isinstance(cc_err, ResourceClosed) and self._name:
                    if (
                        self._following_local is not None
                        and self._scheduler.snapshot().closed
                    ):
                        raise _LocalRouteClosed() from exc
                    raise ResourceClosed(
                        f"route {self._name!r} is closed",
                        details={'route': self._name},
                    ) from exc
                if cc_err is not None:
                    raise cc_err from exc
            if str(exc) == 'route closed':
                from ...error import ResourceClosed
                if (
                    self._following_local is not None
                    and self._scheduler.snapshot().closed
                ):
                    raise _LocalRouteClosed() from exc
                details = {'route': self._name} if self._name else {}
                message = f"route {self._name!r} is closed" if self._name else 'route is closed'
                raise ResourceClosed(message, details=details) from exc
            raise
        try:
            return method(*args)
        finally:
            guard.__exit__(None, None, None)

    def connection_observation(self):
        """Read the current physical IPC connection without acquiring another."""
        with self._close_lock:
            if self._closed:
                from ...error import ResourceClosed
                raise ResourceClosed('CRM proxy is closed')
            if self._mode == 'thread' and self._following_local is None:
                from ...error import UnsupportedCallMode
                raise UnsupportedCallMode('connection observation requires a physical IPC connection')
            state = self._following_local
            if state is not None:
                if not isinstance(state.target, CRMProxy):
                    from ...error import UnsupportedCallMode
                    raise UnsupportedCallMode('connection observation requires a physical IPC connection')
                return _native_call(state.target._client.connection_observation)
            return _native_call(self._client.connection_observation)

    def terminate(self) -> None:
        """Release the proxy and invoke cleanup callback if set."""
        with self._close_lock:
            if self._closed:
                return
            self._closed_event.set()
            callback = self._on_terminate
            self._on_terminate = None
            owned_client = self._client if self._mode == 'thread' else None
            shared_target = self._following_local
            self._following_local = None
        try:
            if owned_client is not None:
                owned_client.close()
            if callback is not None:
                callback()
        finally:
            del shared_target


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


def connection_observation(proxy):
    """Observe connection facts; logical proxy close need not close a shared IPC stream."""
    client = getattr(proxy, 'client', None)
    if not isinstance(client, CRMProxy):
        raise TypeError('cc.connection_observation() requires a connected CRM instance')
    return client.connection_observation()


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
