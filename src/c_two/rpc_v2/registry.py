"""Process-level CRM registry — the SOTA ``cc.register / cc.connect`` API.

Provides a singleton :class:`_ProcessRegistry` that manages:

1. **CRM registration** — registers CRM objects to a process-level
   :class:`ServerV2`, making them accessible via IPC.
2. **Thread preference** — ``connect()`` returns a zero-serialization
   :class:`ICRMProxy.thread_local` when the target CRM lives in the
   same process.
3. **Client pooling** — remote connections reuse :class:`SharedClient`
   instances via :class:`ClientPool`.

Usage::

    import c_two as cc

    # Register CRMs with explicit names
    cc.register(IGrid, grid_instance, name='grid')

    # Connect (same process → thread-local, no serialization)
    icrm = cc.connect(IGrid, name='grid')
    result = icrm.subdivide_grids([1], [0])

    # Close the connection
    cc.close(icrm)

    # Unregister when done
    cc.unregister('grid')

Module-level functions (:func:`register`, :func:`connect`, etc.) delegate
to the global :class:`_ProcessRegistry` singleton.
"""
from __future__ import annotations

import atexit
import logging
import os
import threading
import uuid
from dataclasses import dataclass
from typing import TypeVar

from .config import settings
from .pool import ClientPool
from .proxy import ICRMProxy
from .scheduler import ConcurrencyConfig
from .server import ServerV2

ICRM = TypeVar('ICRM')
logger = logging.getLogger(__name__)


@dataclass
class _Registration:
    """Bookkeeping for a locally registered CRM."""

    name: str
    icrm_class: type
    crm_instance: object
    concurrency: ConcurrencyConfig | None


class _ProcessRegistry:
    """Process-level singleton managing CRM registration and discovery.

    Normally accessed via the module-level :func:`register` /
    :func:`connect` / :func:`unregister` functions.
    """

    _instance: _ProcessRegistry | None = None
    _instance_lock = threading.Lock()

    @classmethod
    def get(cls) -> _ProcessRegistry:
        """Return the global singleton, creating it on first access."""
        if cls._instance is None:
            with cls._instance_lock:
                if cls._instance is None:
                    cls._instance = cls()
        return cls._instance

    @classmethod
    def reset(cls) -> None:
        """Destroy the global singleton (for testing / process exit)."""
        with cls._instance_lock:
            inst = cls._instance
            cls._instance = None
        if inst is not None:
            inst.shutdown()

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._registrations: dict[str, _Registration] = {}
        self._server: ServerV2 | None = None
        self._server_address: str | None = None
        self._explicit_address: str | None = None
        self._pool = ClientPool()

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def set_address(self, address: str) -> None:
        """Set the IPC server address programmatically.

        Priority: ``set_address()`` > ``C2_IPC_ADDRESS`` env var > auto.

        Must be called **before** any :func:`register` call.  Raises
        :class:`RuntimeError` if CRMs are already registered.

        Parameters
        ----------
        address:
            IPC address (e.g. ``'ipc-v3://my_server'``).
        """
        with self._lock:
            if self._server is not None:
                raise RuntimeError(
                    'Cannot set address after CRMs have been registered. '
                    'Call set_address() before register().',
                )
            self._explicit_address = address

    def register(
        self,
        icrm_class: type,
        crm_instance: object,
        *,
        name: str,
        concurrency: ConcurrencyConfig | None = None,
    ) -> str:
        """Register a CRM, making it available via :func:`connect`.

        On the first call, a :class:`ServerV2` is created and started
        automatically (lazy init).

        Parameters
        ----------
        icrm_class:
            ``@cc.icrm``-decorated interface class.
        crm_instance:
            Concrete CRM object that implements *icrm_class*.
        name:
            Unique routing name for this CRM instance.
        concurrency:
            Optional concurrency configuration for the scheduler.

        Returns
        -------
        str
            The *name* string (echoed back for convenience).
        """
        with self._lock:
            if name in self._registrations:
                raise ValueError(f'Name already registered: {name!r}')

            # Lazy-init server on first registration.
            if self._server is None:
                addr = (
                    self._explicit_address
                    or settings.ipc_address
                    or self._auto_address()
                )
                self._server = ServerV2(bind_address=addr)
                self._server_address = addr

            self._server.register_crm(icrm_class, crm_instance, concurrency, name=name)
            self._registrations[name] = _Registration(
                name=name,
                icrm_class=icrm_class,
                crm_instance=crm_instance,
                concurrency=concurrency,
            )

            # Start server if not yet running.
            if not self._server._started.is_set():
                self._server.start()

        logger.debug('Registered CRM %s at %s', name, self._server_address)
        return name

    def connect(
        self,
        icrm_class: type[ICRM],
        *,
        name: str,
        address: str | None = None,
    ) -> ICRM:
        """Obtain an ICRM instance connected to a CRM.

        **Thread preference**: when the target *name* is registered in
        this process and no explicit *address* is given, returns a
        zero-serialization local proxy.

        Parameters
        ----------
        icrm_class:
            ``@cc.icrm``-decorated interface class.
        name:
            Routing name of the target CRM.
        address:
            Explicit IPC server address (e.g. ``'ipc-v3://remote'``).
            If ``None``, looks up the local registry.

        Returns
        -------
        object
            An ICRM instance with ``.client`` set to an
            :class:`ICRMProxy`.
        """
        with self._lock:
            local = self._registrations.get(name)

        if address is None and local is not None:
            # Thread preference — same process, no serialization.
            proxy = ICRMProxy.thread_local(local.crm_instance)
        elif address is not None:
            # Remote IPC via pooled SharedClient.
            client = self._pool.acquire(address, try_v2=True)
            proxy = ICRMProxy.ipc(
                client,
                name,
                on_terminate=lambda addr=address: self._pool.release(addr),
            )
        else:
            raise LookupError(
                f'Name {name!r} is not registered locally '
                f'and no address was provided',
            )

        icrm = icrm_class()
        icrm.client = proxy
        return icrm

    def close(self, icrm: object) -> None:
        """Close a connection obtained from :func:`connect`.

        Terminates the underlying proxy and releases pool references.
        """
        proxy = getattr(icrm, 'client', None)
        if proxy is not None and hasattr(proxy, 'terminate'):
            proxy.terminate()

    def unregister(self, name: str) -> None:
        """Remove a CRM from the registry and server.

        Parameters
        ----------
        name:
            The routing name used during :func:`register`.
        """
        with self._lock:
            reg = self._registrations.pop(name, None)
            if reg is None:
                raise KeyError(f'Name not registered: {name!r}')
            if self._server is not None:
                self._server.unregister_crm(name)

    def get_server_address(self) -> str | None:
        """IPC address of the auto-created server, or ``None``."""
        return self._server_address

    @property
    def names(self) -> list[str]:
        """List of currently registered routing names."""
        with self._lock:
            return list(self._registrations.keys())

    def shutdown(self) -> None:
        """Full cleanup — shuts down server, terminates pooled clients.

        Called automatically at process exit via :func:`atexit`.
        """
        with self._lock:
            server = self._server
            self._server = None
            self._server_address = None
            self._explicit_address = None
            self._registrations.clear()

        if server is not None:
            try:
                server.shutdown()
            except Exception:
                logger.warning('Error shutting down ServerV2', exc_info=True)

        self._pool.shutdown_all()

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    @staticmethod
    def _auto_address() -> str:
        return f'ipc-v3://cc_auto_{os.getpid()}_{uuid.uuid4().hex[:8]}'


# ------------------------------------------------------------------
# Module-level API (delegates to singleton)
# ------------------------------------------------------------------

def set_address(address: str) -> None:
    """Set the IPC server address before registering any CRM.

    Priority: ``set_address()`` > ``C2_IPC_ADDRESS`` env var > auto.

    See :meth:`_ProcessRegistry.set_address`.
    """
    _ProcessRegistry.get().set_address(address)


def register(
    icrm_class: type,
    crm_instance: object,
    *,
    name: str,
    concurrency: ConcurrencyConfig | None = None,
) -> str:
    """Register a CRM in the current process.

    See :meth:`_ProcessRegistry.register`.
    """
    return _ProcessRegistry.get().register(
        icrm_class,
        crm_instance,
        name=name,
        concurrency=concurrency,
    )


def connect(
    icrm_class: type[ICRM],
    *,
    name: str,
    address: str | None = None,
) -> ICRM:
    """Obtain an ICRM proxy for a CRM.

    See :meth:`_ProcessRegistry.connect`.
    """
    return _ProcessRegistry.get().connect(icrm_class, name=name, address=address)


def close(icrm: object) -> None:
    """Close a connection obtained from :func:`connect`.

    See :meth:`_ProcessRegistry.close`.
    """
    _ProcessRegistry.get().close(icrm)


def unregister(name: str) -> None:
    """Remove a CRM from the registry.

    See :meth:`_ProcessRegistry.unregister`.
    """
    _ProcessRegistry.get().unregister(name)


def server_address() -> str | None:
    """IPC address of the auto-created server, or ``None``."""
    return _ProcessRegistry.get().get_server_address()


def shutdown() -> None:
    """Full cleanup — shuts down server and pooled clients.

    See :meth:`_ProcessRegistry.shutdown`.
    """
    _ProcessRegistry.get().shutdown()


# Auto-cleanup on process exit.
atexit.register(lambda: _ProcessRegistry.reset())
