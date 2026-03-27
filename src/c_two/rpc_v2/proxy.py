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

from typing import Any, Callable


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
        '_closed', '_on_terminate',
    )

    # ------------------------------------------------------------------
    # Factory classmethods
    # ------------------------------------------------------------------

    @classmethod
    def thread_local(
        cls,
        crm_instance: object,
        *,
        on_terminate: Callable[[], None] | None = None,
    ) -> ICRMProxy:
        """Create a thread-local proxy (same process, no serialization).

        ``supports_direct_call`` is ``True`` — the ``auto_transfer``
        wrapper will call :meth:`call_direct` instead of :meth:`call`.
        """
        proxy = object.__new__(cls)
        proxy._mode = 'thread'
        proxy._crm = crm_instance
        proxy._client = None
        proxy._name = ''
        proxy._closed = False
        proxy._on_terminate = on_terminate
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
        proxy._closed = False
        proxy._on_terminate = on_terminate
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
        proxy._closed = False
        proxy._on_terminate = on_terminate
        return proxy

    # ------------------------------------------------------------------
    # Client interface (compatible with rpc.Client)
    # ------------------------------------------------------------------

    @property
    def supports_direct_call(self) -> bool:
        """``True`` for thread-local proxies (skip serialization)."""
        return self._mode == 'thread'

    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        """Send a serialized CRM call (IPC or HTTP mode).

        Raises :class:`NotImplementedError` in thread-local mode — use
        :meth:`call_direct` instead.
        """
        if self._closed:
            raise RuntimeError('Proxy is closed')
        if self._mode in ('ipc', 'http'):
            return self._client.call(method_name, data, name=self._name)
        raise NotImplementedError(
            'call() not available in thread-local mode; use call_direct()',
        )

    def call_direct(self, method_name: str, args: tuple) -> Any:
        """Call CRM method directly with Python objects (thread-local mode).

        Raises :class:`NotImplementedError` in IPC mode.
        """
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
        return method(*args)

    def relay(self, event_bytes: bytes) -> bytes:
        """Relay raw wire bytes to the server (IPC mode only)."""
        if self._closed:
            raise RuntimeError('Proxy is closed')
        if self._mode == 'ipc':
            return self._client.relay(event_bytes)
        raise NotImplementedError(
            'relay() not available in thread-local mode',
        )

    def terminate(self) -> None:
        """Release the proxy and invoke cleanup callback if set."""
        if self._closed:
            return
        self._closed = True
        if self._on_terminate is not None:
            self._on_terminate()
