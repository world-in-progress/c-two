"""Reference-counted pool for :class:`SharedClient` instances.

Instead of each ICRM consumer creating its own connection + 256 MB buddy
pool, all consumers connecting to the same server share a single
:class:`SharedClient`.  The pool manages lifecycle via reference counting
with a configurable grace period (default 60 s) to avoid thrashing.

Usage::

    pool = ClientPool.instance()
    client = pool.acquire('ipc-v3://my_region')
    try:
        result = client.call('some_method', data)
    finally:
        pool.release('ipc-v3://my_region')
"""
from __future__ import annotations

import logging
import threading
import time

from .core import SharedClient
from ..ipc.frame import IPCConfig

logger = logging.getLogger(__name__)

_DEFAULT_GRACE_SECONDS = 60.0


class ClientPool:
    """Process-level pool of :class:`SharedClient` instances.

    Normally accessed via :meth:`instance` (singleton), but can be
    instantiated directly for testing.
    """

    _global_instance: ClientPool | None = None
    _global_lock = threading.Lock()

    def __init__(
        self,
        grace_seconds: float = _DEFAULT_GRACE_SECONDS,
        default_config: IPCConfig | None = None,
    ):
        self._grace_seconds = grace_seconds
        self._default_config = default_config
        self._lock = threading.Lock()
        self._clients: dict[str, SharedClient] = {}
        self._refcounts: dict[str, int] = {}
        self._timers: dict[str, threading.Timer] = {}

    @classmethod
    def instance(cls) -> ClientPool:
        """Return the process-level singleton."""
        if cls._global_instance is None:
            with cls._global_lock:
                if cls._global_instance is None:
                    cls._global_instance = cls()
        return cls._global_instance

    @classmethod
    def reset_instance(cls) -> None:
        """Destroy the global singleton (for testing only)."""
        with cls._global_lock:
            inst = cls._global_instance
            cls._global_instance = None
        if inst is not None:
            inst.shutdown_all()

    def set_default_config(self, config: IPCConfig) -> None:
        """Update the default IPC config for newly created clients."""
        self._default_config = config

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def acquire(
        self,
        server_address: str,
        ipc_config: IPCConfig | None = None,
    ) -> SharedClient:
        """Get a :class:`SharedClient` for *server_address*, creating if needed.

        Increments the reference count.  Caller must call :meth:`release`
        when done.
        """
        with self._lock:
            # Cancel any pending destroy timer.
            timer = self._timers.pop(server_address, None)
            if timer is not None:
                timer.cancel()

            client = self._clients.get(server_address)
            if client is not None:
                self._refcounts[server_address] += 1
                return client

            # Create new client.
            config = ipc_config or self._default_config
            client = SharedClient(server_address, config)
            client.connect()
            self._clients[server_address] = client
            self._refcounts[server_address] = 1
            return client

    def release(self, server_address: str) -> None:
        """Decrement reference count.  When it reaches 0, schedule a
        grace-period destroy.
        """
        with self._lock:
            count = self._refcounts.get(server_address, 0)
            if count <= 0:
                logger.warning('release() called with no matching acquire for %s',
                               server_address)
                return
            count -= 1
            self._refcounts[server_address] = count

            if count == 0:
                timer = threading.Timer(
                    self._grace_seconds,
                    self._destroy,
                    args=(server_address,),
                )
                timer.daemon = True
                timer.name = f'c2-pool-grace-{server_address}'
                self._timers[server_address] = timer
                timer.start()

    def shutdown_all(self) -> None:
        """Terminate all clients immediately (for testing / process exit)."""
        with self._lock:
            for timer in self._timers.values():
                timer.cancel()
            self._timers.clear()
            clients = list(self._clients.values())
            self._clients.clear()
            self._refcounts.clear()

        for client in clients:
            try:
                client.terminate()
            except Exception:
                logger.warning('Failed to terminate client', exc_info=True)

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _destroy(self, server_address: str) -> None:
        """Called by grace timer after all references released."""
        with self._lock:
            self._timers.pop(server_address, None)
            # Re-check: could have been re-acquired during grace period.
            if self._refcounts.get(server_address, 0) > 0:
                return
            client = self._clients.pop(server_address, None)
            self._refcounts.pop(server_address, None)

        if client is not None:
            logger.debug('Destroying SharedClient for %s (grace period expired)',
                         server_address)
            try:
                client.terminate()
            except Exception:
                logger.warning('Failed to terminate client %s', server_address,
                               exc_info=True)

    # ------------------------------------------------------------------
    # Introspection (for testing)
    # ------------------------------------------------------------------

    def active_count(self) -> int:
        with self._lock:
            return len(self._clients)

    def refcount(self, server_address: str) -> int:
        with self._lock:
            return self._refcounts.get(server_address, 0)

    def has_client(self, server_address: str) -> bool:
        with self._lock:
            return server_address in self._clients
