"""Unified client-compatible proxy for ICRM consumers.

Implements the same interface as :class:`rpc.Client` — assign to
``icrm.client`` and the existing ``auto_transfer`` machinery works
transparently.

Three modes:

- **thread-local**: ``supports_direct_call = True``.  Calls CRM methods
  directly via ``call_direct(method_name, args)`` — zero serialization.
- **ipc**: ``supports_direct_call = False``.  Delegates to a
  :class:`SharedClient` with routing-name-based ``call()`` / ``relay()``.
- **http**: ``supports_direct_call = False``.  Delegates to an
  :class:`HttpClient` for cross-node access via an HTTP relay.

Usage::

    # Thread-local (same process, skip serialization):
    proxy = ICRMProxy.thread_local(crm_instance)
    icrm = IHello()
    icrm.client = proxy
    icrm.greeting('World')       # → crm_instance.greeting('World')

    # IPC (cross-process via SharedClient):
    proxy = ICRMProxy.ipc(shared_client, 'hello')
    icrm = IHello()
    icrm.client = proxy
    icrm.greeting('World')       # → shared_client.call('greeting', ..., name='hello')

    # HTTP (cross-node via HttpClient):
    proxy = ICRMProxy.http(http_client, 'hello')
    icrm = IHello()
    icrm.client = proxy
    icrm.greeting('World')       # → http_client.call('greeting', ..., name='hello')
"""
from __future__ import annotations

import threading
from typing import Any, Callable, TYPE_CHECKING

from ...crm.meta import MethodAccess

if TYPE_CHECKING:
    from ..server.scheduler import Scheduler


class ICRMProxy:
    """Client-compatible proxy for ICRM consumers.

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
        '_scheduler', '_access_map', '_rust_ipc', '_rust_http',
    )

    # ------------------------------------------------------------------
    # Factory classmethods
    # ------------------------------------------------------------------

    @classmethod
    def thread_local(
        cls,
        crm_instance: object,
        *,
        scheduler: Scheduler | None = None,
        access_map: dict[str, MethodAccess] | None = None,
        on_terminate: Callable[[], None] | None = None,
    ) -> ICRMProxy:
        """Create a thread-local proxy (same process, no serialization).

        ``supports_direct_call`` is ``True`` — the ``auto_transfer``
        wrapper will call :meth:`call_direct` instead of :meth:`call`.

        Parameters
        ----------
        scheduler:
            Optional :class:`Scheduler` for read/write concurrency control.
            When provided, ``call_direct`` wraps method execution in
            the scheduler's execution guard.
        access_map:
            Mapping from method name → :class:`MethodAccess`.  Required
            when *scheduler* is provided.
        """
        proxy = object.__new__(cls)
        proxy._mode = 'thread'
        proxy._crm = crm_instance
        proxy._client = None
        proxy._name = ''
        proxy._close_lock = threading.Lock()
        proxy._closed = False
        proxy._on_terminate = on_terminate
        proxy._scheduler = scheduler
        proxy._access_map = access_map
        proxy._rust_ipc = False
        proxy._rust_http = False
        return proxy

    @classmethod
    def ipc(
        cls,
        shared_client: Any,  # SharedClient — avoid circular import
        name: str,
        *,
        on_terminate: Callable[[], None] | None = None,
    ) -> ICRMProxy:
        """Create an IPC proxy (cross-process via SharedClient).

        Calls are routed to the CRM registered under *name* on the
        remote server.
        """
        proxy = object.__new__(cls)
        proxy._mode = 'ipc'
        proxy._crm = None
        proxy._client = shared_client
        proxy._name = name
        proxy._close_lock = threading.Lock()
        proxy._closed = False
        proxy._on_terminate = on_terminate
        proxy._scheduler = None
        proxy._access_map = None
        # Detect Rust-backed client (route_names is Rust-only).
        proxy._rust_ipc = hasattr(shared_client, 'route_names')
        proxy._rust_http = False
        if proxy._rust_ipc and not name:
            names = shared_client.route_names()
            if names:
                proxy._name = names[0]
        return proxy

    @classmethod
    def http(
        cls,
        http_client: Any,  # HttpClient — avoid circular import
        name: str,
        *,
        on_terminate: Callable[[], None] | None = None,
    ) -> ICRMProxy:
        """Create an HTTP proxy (cross-node via HttpClient + relay).

        Calls are sent to the relay server as
        ``POST /{name}/{method_name}`` with the serialized payload.
        """
        proxy = object.__new__(cls)
        proxy._mode = 'http'
        proxy._crm = None
        proxy._client = http_client
        proxy._name = name
        proxy._close_lock = threading.Lock()
        proxy._closed = False
        proxy._on_terminate = on_terminate
        proxy._scheduler = None
        proxy._access_map = None
        proxy._rust_ipc = False
        # Detect Rust-backed HTTP client (has no 'name' keyword on call).
        proxy._rust_http = type(http_client).__name__ == 'RustHttpClient'
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

    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        """Send a serialized CRM call (IPC or HTTP mode).

        Raises :class:`NotImplementedError` in thread-local mode — use
        :meth:`call_direct` instead.
        """
        with self._close_lock:
            if self._closed:
                raise RuntimeError('Proxy is closed')
        if self._mode == 'ipc':
            if self._rust_ipc:
                try:
                    return self._client.call(
                        self._name, method_name, data or b'',
                    )
                except Exception as exc:
                    error_bytes = getattr(exc, 'error_bytes', None)
                    if error_bytes is not None:
                        from ...error import CCError
                        cc_err = CCError.deserialize(memoryview(error_bytes))
                        if cc_err is not None:
                            raise cc_err from exc
                    raise
            return self._client.call(method_name, data, name=self._name)
        if self._mode == 'http':
            if self._rust_http:
                try:
                    return self._client.call(
                        self._name, method_name, data or b'',
                    )
                except Exception as exc:
                    error_bytes = getattr(exc, 'error_bytes', None)
                    if error_bytes is not None:
                        from ...error import CCError
                        cc_err = CCError.deserialize(memoryview(error_bytes))
                        if cc_err is not None:
                            raise cc_err from exc
                    raise
            return self._client.call(method_name, data, name=self._name)
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
        method = getattr(self._crm, method_name, None)
        if method is None:
            raise AttributeError(
                f'{type(self._crm).__name__} has no method {method_name!r}',
            )
        if self._scheduler is None:
            return method(*args)
        access = self._access_map.get(method_name, MethodAccess.WRITE) if self._access_map else MethodAccess.WRITE
        with self._scheduler.execution_guard(access):
            return method(*args)

    def relay(self, event_bytes: bytes) -> bytes:
        """Relay raw wire bytes to the server (IPC mode only)."""
        with self._close_lock:
            if self._closed:
                raise RuntimeError('Proxy is closed')
        if self._mode == 'ipc':
            return self._client.relay(event_bytes)
        raise NotImplementedError(
            'relay() not available in thread-local mode',
        )

    def terminate(self) -> None:
        """Release the proxy and invoke cleanup callback if set."""
        with self._close_lock:
            if self._closed:
                return
            self._closed = True
        if self._on_terminate is not None:
            self._on_terminate()
