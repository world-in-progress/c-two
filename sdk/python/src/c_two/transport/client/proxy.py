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
from dataclasses import dataclass
from typing import Any, Callable, Literal, TYPE_CHECKING

from ... import error
from ..._call_context import CallContext, _UNSET, _run_in_call_context

if TYPE_CHECKING:
    from ..server.scheduler import Scheduler


class _LocalRouteClosed(Exception):
    """Native admission closed before a same-process resource was entered."""


@dataclass(frozen=True, slots=True)
class CallOutcome:
    """Snapshot of a keyed unary call in the route's current registration.

    ``in_flight`` includes a call still receiving its request. Querying never
    waits for the resource to finish or executes it.
    """

    status: Literal['in_flight', 'succeeded', 'failed', 'unknown']
    method_name: str | None
    result_available: bool
    result_unavailable_reason: Literal['not_retained', 'evicted'] | None
    error_code: int | None
    error_details_available: bool


def _closed_proxy_error() -> error.ResourceClosed:
    """Local refusal for an explicitly closed CRM proxy (retry-safe)."""
    return error.ResourceClosed(
        'CRM proxy is closed', transport_phase='pre_dispatch',
    )


class _FollowingLocalRoute:
    """Target shared by the views of one registered same-process connection."""

    def __init__(self, runtime, session, crm_class, contract, name, resource, scheduler):
        self.lock = threading.Lock()
        self.acquire_gate = threading.Lock()
        self.runtime = runtime
        self.session = session
        self.crm_class = crm_class
        self.contract = contract
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
        '_call_budget', '_following_local',
        '_topic_bindings',
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
        topic_bindings: Any = None,
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
        proxy._call_budget = None
        proxy._following_local = following_local
        proxy._topic_bindings = topic_bindings
        return proxy

    @classmethod
    def ipc(
        cls,
        client: Any,
        name: str,
        *,
        on_terminate: Callable[[], None] | None = None,
        local_copy_tracker: Any = None,
        topic_bindings: Any = None,
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
        proxy._call_budget = None
        proxy._following_local = None
        proxy._topic_bindings = topic_bindings
        return proxy

    @classmethod
    def http(
        cls,
        http_client: Any,
        name: str,
        *,
        on_terminate: Callable[[], None] | None = None,
        local_copy_tracker: Any = None,
        topic_bindings: Any = None,
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
        proxy._call_budget = None
        proxy._following_local = None
        proxy._topic_bindings = topic_bindings
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
            raise _closed_proxy_error()

        target = state.target
        if isinstance(target, CRMProxy):
            return self._remote_snapshot(target, budget)
        resource, scheduler = target if target is not None else state.local
        if scheduler is None or not scheduler.is_closed:
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
            if not isinstance(current, CRMProxy) and local[1].is_closed:
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
                        transport_phase='pre_dispatch',
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
                    if target._scheduler is not None and target._scheduler.is_closed:
                        target.terminate()
                        from ...error import ResourceClosed
                        raise ResourceClosed(
                            f"route {state.name!r} is closed",
                            details={'route': state.name},
                            transport_phase='pre_dispatch',
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
            raise _closed_proxy_error()
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
                raise _closed_proxy_error()
            client = self._client
            if client is None:
                client = remote._client
                if self._call_options is not None:
                    client = _native_call(client.with_call_options, self._call_options)
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

    def with_call_options(self, *, timeout=_UNSET, metadata=_UNSET) -> CRMProxy:
        """Create an independent connection view with Core-owned call options."""
        from ..._native import NativeCallOptions

        with self._close_lock:
            if self._closed:
                raise _closed_proxy_error()
            view = self._snapshot()
        # Read user-supplied options outside the connection lock. The snapshot
        # already keeps the shared route alive if the parent closes meanwhile.
        options = (view._call_options or NativeCallOptions()).with_overrides(
            _UNSET, timeout, metadata,
        )
        view._close_lock = threading.Lock()
        view._closed_event = threading.Event()
        view._call_options = options
        if view._mode == 'thread':
            view._client = None
        else:
            with self._close_lock:
                if self._closed:
                    raise _closed_proxy_error()
                view._client = _native_call(self._client.with_call_options, options)
            view._on_terminate = view._client.close
        return view

    def begin_call(self, method_name: str):
        """Reserve the Core operation before Python input serialization."""
        with self._close_lock:
            if self._closed:
                raise _closed_proxy_error()
        if self._call_budget is not None:
            budget = self._call_budget
            self._call_budget = None
            return _native_call(self._client.begin_call_with_budget, method_name, budget)
        return _native_call(self._client.begin_call, method_name)

    def prepare_call(self, method_name: str):
        """Project one reserved native call into the transfer interface."""
        return _PreparedClient(self.begin_call(method_name))

    def _outcome_target(self) -> CRMProxy:
        """Resolve the encoded route for an outcome operation."""
        with self._close_lock:
            if self._closed:
                raise _closed_proxy_error()
        if self._mode != 'thread':
            return self

        state = self._following_local
        if state is None or not self._name:
            raise error.UnsupportedCallMode(
                'call outcomes require a thread-local proxy with a registered Runtime route',
                transport_phase='pre_dispatch',
            )
        budget = self.start_call_budget()
        attempt = _native_call(budget.connect_attempt)
        _native_call(state.session.check_active, connect_attempt=attempt)
        address = state.session.server_address
        if address is None:
            raise error.ResourceClosed(
                f'Host for route {state.name!r} is closed', transport_phase='pre_dispatch',
            )
        client = _native_call(
            state.session.acquire_ipc_client,
            address,
            state.name,
            *state.contract.native_args(),
            connect_attempt=attempt,
        )
        if self._call_options is not None:
            client = _native_call(client.with_call_options, self._call_options)
        target = CRMProxy.ipc(client, state.name, local_copy_tracker=self._local_copy_tracker)
        target._call_budget = budget
        return target

    def _prepare_keyed_call(self, method_name: str, call_id: str):
        if self._call_budget is not None:
            return _PreparedClient(_native_call(
                self._client.begin_call_with_budget, method_name, self._call_budget, call_id=call_id,
            ))
        return _PreparedClient(_native_call(self._client.begin_call, method_name, call_id=call_id))

    def _recover_call(self, method_name: str, call_id: str):
        return _native_call(self._client.recover_call, method_name, call_id, self._call_budget)

    def _query_call(self, call_id: str):
        return _native_call(self._client.query_call, call_id, self._call_budget)

    def _admit_local(self, target, operation, method_index):
        from ..._native import NativeCallOptions

        options = self._call_options or NativeCallOptions()
        if self._following_local is None or self._scheduler is None:
            return CallContext(self._name, target, operation, None, options.metadata)
        try:
            return _native_call(
                self._scheduler._registration.admit_local, target, operation, method_index, options,
            )
        except error.ResourceClosed as exc:
            raise _LocalRouteClosed() from exc

    def begin_stream(self, method_name: str):
        """Reserve the native stream call before encoding its input."""
        with self._close_lock:
            if self._closed:
                raise _closed_proxy_error()
        if self._call_budget is not None:
            budget = self._call_budget
            self._call_budget = None
            return _PreparedStream(
                _native_call(self._client.begin_stream_with_budget, method_name, budget),
            )
        return _PreparedStream(_native_call(self._client.begin_stream, method_name))

    def open_direct_stream(self, method_name: str, args: tuple, *, output):
        from ..._native import NativeCallOptions
        from ...crm.stream import _ResourceStream
        with self._close_lock:
            if self._closed:
                raise _closed_proxy_error()
        try:
            _native_call((self._call_options or NativeCallOptions()).validate_thread)
        except Exception as exc:
            from ...error import UnsupportedCallMode
            if (
                issubclass(type(exc), UnsupportedCallMode)
                and self._following_local is not None
                and self._scheduler is not None
                and self._scheduler.snapshot().closed
            ):
                raise _LocalRouteClosed() from exc
            raise
        if self._scheduler is None:
            raise RuntimeError('same-process streams require a registered native route')
        method_index = self._scheduler.method_idx(method_name)
        context = self._admit_local(method_name, 'server_stream', method_index)
        method = getattr(self._crm, method_name)
        try:
            return _native_call(
                self._scheduler.open_stream,
                method_index,
                lambda: _ResourceStream(
                    method, args=args, encoded=False, output=output, call_context=context,
                ),
            )
        except Exception as exc:
            from ...error import ResourceClosed
            if (
                issubclass(type(exc), ResourceClosed)
                and self._following_local is not None
                and self._scheduler.snapshot().closed
            ):
                raise _LocalRouteClosed() from exc
            raise

    def subscribe(
        self,
        topic: str,
        *,
        start: str = 'latest',
        on_lag: str = 'skip',
        timeout=...,
        hold: bool = False,
    ):
        """Attach to one topic declared on this proxy's contract.

        Same-process connections attach directly to the route's current
        registration; remote connections subscribe over IPC 18 / relay HTTP.
        Each call creates one subscription spanning at most one registration:
        unregistering the route or stopping the host ends it with
        ``CallCancelled`` (718), a lagging ``on_lag='error'`` policy ends it
        with ``SubscriptionLagged`` (720), and a later ``subscribe()`` follows
        the current registration.

        ``hold=True`` retains each delivery's local copy, yielding
        ``Delivery[HeldResult[T]]`` whose ``.value`` owns the release.
        """
        from ..topic import Subscription

        with self._close_lock:
            if self._closed:
                raise _closed_proxy_error()
        state = self._following_local
        target = self.resolve_for_call()
        if target._mode == 'thread' and state is None:
            raise error.UnsupportedCallMode(
                'topic subscription requires a registered route; this thread-local proxy has none',
            )
        binding = self._require_topic_binding(topic)
        budget = None
        while target._mode == 'thread':
            try:
                if timeout is ...:
                    from ..._native import NativeCallOptions

                    _native_call(
                        (target._call_options or NativeCallOptions()).validate_thread,
                    )
                elif timeout is not None:
                    raise error.UnsupportedCallMode(
                        'finite subscription timeout is unsupported for '
                        'same-process subscriptions',
                    )
                target._admit_local(topic, 'subscribe', None)
                native = _native_call(
                    target._scheduler._registration.subscribe_local, topic, start, on_lag,
                )
            except (_LocalRouteClosed, error.ResourceClosed):
                if budget is None:
                    budget = self.start_call_budget()
                target = self.resolve_for_call(budget)
                continue
            return Subscription(
                native,
                binding,
                held=hold,
                tracker=self._local_copy_tracker,
                route_name=self._name,
                local=True,
            )
        native = _native_call(
            target._client.subscribe,
            topic,
            start,
            on_lag,
            timeout,
        )
        return Subscription(
            native,
            binding,
            held=hold,
            tracker=self._local_copy_tracker,
            route_name=self._name,
            local=False,
        )

    def _require_topic_binding(self, topic: str):
        binding = (self._topic_bindings or {}).get(topic)
        if binding is None:
            from ...error import ProtocolViolation
            raise ProtocolViolation(
                f'topic {topic!r} is not declared on the contract '
                f'of route {self._name!r}',
            )
        return binding

    def call(self, method_name: str, data: bytes | bytearray | memoryview | None = None) -> bytes:
        """Send a serialized CRM call (IPC or HTTP mode).

        Raises :class:`NotImplementedError` in thread-local mode — use
        :meth:`call_direct` instead.
        """
        with self._close_lock:
            if self._closed:
                raise _closed_proxy_error()
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
                raise _closed_proxy_error()
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
                issubclass(type(exc), UnsupportedCallMode)
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
        method_idx = self._scheduler.method_idx(method_name) if self._scheduler else None
        context = self._admit_local(method_name, 'unary', method_idx)
        try:
            if self._scheduler is None:
                return _run_in_call_context(context, _invoke_local, (method, args))
            try:
                guard = _native_call(self._scheduler.execution_guard, method_idx)
                # Final admission can fail after acquire while Python waits to run.
                # Normalize that native error before entering resource code.
                _native_call(guard.__enter__)
            except error.ResourceClosed as exc:
                if (
                    self._following_local is not None
                    and self._scheduler.snapshot().closed
                ):
                    raise _LocalRouteClosed() from exc
                if self._name:
                    raise error.ResourceClosed(
                        f"route {self._name!r} is closed",
                        details={'route': self._name},
                        transport_phase='pre_dispatch',
                    ) from exc
                raise
            try:
                return _run_in_call_context(context, _invoke_local, (method, args))
            finally:
                guard.__exit__(None, None, None)
        finally:
            del context

    def connection_observation(self):
        """Read the current physical IPC connection without acquiring another."""
        with self._close_lock:
            if self._closed:
                raise _closed_proxy_error()
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


def _invoke_local(method, args):
    try:
        return method(*args)
    except Exception as exc:
        cls = type(exc)
        if issubclass(cls, error.CoreError):
            # A downstream admission failure describes the inner call only:
            # re-raise a clean copy instead of mutating the business's object.
            raise (error._callback_semantic_copy(exc) or error._callback_core_copy(exc)) from exc  # type: ignore[arg-type]
        if issubclass(cls, error.CCError):
            raise error._callback_error_copy(exc) from exc  # type: ignore[arg-type]
        if issubclass(cls, error.CCBaseError):
            from ...crm.transferable import _cause_details, _safe_error_message
            raise error.ResourceExecutionError(
                _safe_error_message(exc),
                details=_cause_details(exc),
            ) from exc
        raise
    except BaseException as exc:
        from ...crm.transferable import _cause_details, _safe_error_message
        raise error.ResourceExecutionError(
            _safe_error_message(exc),
            details=_cause_details(exc),
        ) from exc


def _native_call(operation, *args, **kwargs):
    """Project a native failure's canonical semantic error, if it has one."""
    try:
        return operation(*args, **kwargs)
    except Exception as exc:
        projected = error._project_native_error(exc)
        if projected is not None:
            try:
                raise projected from exc
            finally:
                del projected
        raise


class _PreparedClient:
    """One caller-thread serialization followed by the reserved Core call."""

    __slots__ = ('_prepared',)
    _supports_release_input = True

    def __init__(self, prepared):
        self._prepared = prepared

    def call(self, method_name, data=None, *, release_input: bool = False):
        payload = data if data is not None else b''
        if release_input:
            return _native_call(self._prepared.call, payload, release_input=True)
        return _native_call(self._prepared.call, payload)

    def charge_input(self, nbytes):
        return _native_call(self._prepared.charge_input, nbytes)

    def close(self):
        self._prepared.close()


class _PreparedStream:
    """One caller-thread stream preparation projecting native call failures.

    Symmetric to :class:`_PreparedClient`: charging the encoded input and
    starting the stream surface the same canonical semantic projection as
    every other native call site instead of a bare ``CoreError``.
    """

    __slots__ = ('_prepared',)
    _supports_release_input = True

    def __init__(self, prepared):
        self._prepared = prepared

    def charge_input(self, nbytes):
        return _native_call(self._prepared.charge_input, nbytes)

    def stream(self, data=None, *, release_input: bool = False):
        payload = data if data is not None else b''
        if release_input:
            return _native_call(self._prepared.stream, payload, release_input=True)
        return _native_call(self._prepared.stream, payload)

    def close(self):
        self._prepared.close()


def connection_observation(proxy):
    """Observe connection facts; logical proxy close need not close a shared IPC stream."""
    client = getattr(proxy, 'client', None)
    if not isinstance(client, CRMProxy):
        raise TypeError('cc.connection_observation() requires a connected CRM instance')
    return client.connection_observation()


def query_call(proxy, call_id: str, *, timeout=_UNSET) -> CallOutcome:
    """Read the current registration's outcome without taking a business slot.

    Omission inherits the connection's timeout policy. Pass ``None`` to
    wait without a deadline for the query reply.
    """
    from ...crm.transferable import _validate_call_id

    _validate_call_id(call_id)
    client = getattr(proxy, 'client', None)
    if not isinstance(client, CRMProxy):
        raise TypeError('cc.query_call() requires a connected CRM instance')
    if timeout is _UNSET:
        return CallOutcome(**client._outcome_target()._query_call(call_id))
    view = client.with_call_options(timeout=timeout)
    try:
        return CallOutcome(**view._outcome_target()._query_call(call_id))
    finally:
        view.terminate()


def with_call_options(proxy, *, timeout=_UNSET, metadata=_UNSET):
    """Derive a connection view with a timeout in seconds and call metadata.

    Omitted options keep the parent view's values. Use ``timeout=...`` for the
    path default and ``timeout=None`` for unlimited waiting. Metadata entries
    overlay the parent view, and ``metadata=None`` clears them.
    """
    client = getattr(proxy, 'client', None)
    if not isinstance(client, CRMProxy):
        raise TypeError('cc.with_call_options() requires a connected CRM instance')
    view = object.__new__(type(proxy))
    view.__dict__.update(proxy.__dict__)
    view.client = client.with_call_options(timeout=timeout, metadata=metadata)
    return view
