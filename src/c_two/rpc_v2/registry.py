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
import signal
import sys
import threading
import uuid
from dataclasses import dataclass
from typing import TypeVar

import httpx

from .config import settings
from .http_client import HttpClientPool
from .pool import ClientPool
from .proxy import ICRMProxy
from .scheduler import ConcurrencyConfig, Scheduler
from .server import ServerV2

from ..crm.meta import MethodAccess
from ..rpc.ipc.ipc_protocol import IPCConfig

ICRM = TypeVar('ICRM')
log = logging.getLogger(__name__)

_RELAY_TIMEOUT = 5.0


def _physical_memory() -> int | None:
    """Return physical RAM in bytes, or *None* if unavailable."""
    try:
        pages = os.sysconf('SC_PHYS_PAGES')
        page_size = os.sysconf('SC_PAGE_SIZE')
        if pages > 0 and page_size > 0:
            return pages * page_size
    except (ValueError, OSError, AttributeError):
        pass
    return None


@dataclass
class _Registration:
    """Bookkeeping for a locally registered CRM."""

    name: str
    icrm_class: type
    crm_instance: object
    concurrency: ConcurrencyConfig | None
    scheduler: Scheduler | None = None
    access_map: dict[str, MethodAccess] | None = None


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
        self._explicit_ipc_config: IPCConfig | None = None
        self._pool = ClientPool()
        self._http_pool = HttpClientPool()

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

    def set_ipc_config(
        self,
        *,
        segment_size: int | None = None,
        max_segments: int | None = None,
    ) -> None:
        """Set IPC transport parameters programmatically.

        Priority: ``set_ipc_config()`` > ``C2_IPC_SEGMENT_SIZE`` /
        ``C2_IPC_MAX_SEGMENTS`` env vars > defaults.

        Must be called **before** any :func:`register` call.

        Parameters
        ----------
        segment_size:
            Buddy pool segment size in bytes.  Determines the maximum
            single-call payload size.  Default: 256 MB.
        max_segments:
            Maximum number of buddy pool segments (1–255).  Default: 4.
        """
        with self._lock:
            if self._server is not None:
                raise RuntimeError(
                    'Cannot set IPC config after CRMs have been registered. '
                    'Call set_ipc_config() before register().',
                )
            overrides: dict[str, int] = {}
            if segment_size is not None:
                overrides['pool_segment_size'] = segment_size
            if max_segments is not None:
                overrides['max_pool_segments'] = max_segments
            if overrides:
                self._explicit_ipc_config = self._make_ipc_config(**overrides)

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

        If ``C2_RELAY_ADDRESS`` is set, the CRM is also registered with
        the relay server via ``POST /_register``.  A connection failure
        or non-2xx response raises an error.

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
                ipc_cfg = self._build_ipc_config()
                self._server = ServerV2(bind_address=addr, ipc_config=ipc_cfg)
                self._server_address = addr
                # Share the same config with the client pool.
                self._pool.set_default_config(ipc_cfg)

            self._server.register_crm(icrm_class, crm_instance, concurrency, name=name)
            scheduler, access_map = self._server.get_slot_info(name)
            self._registrations[name] = _Registration(
                name=name,
                icrm_class=icrm_class,
                crm_instance=crm_instance,
                concurrency=concurrency,
                scheduler=scheduler,
                access_map=access_map,
            )

            # Start server if not yet running.
            if not self._server.is_started():
                self._server.start()

            server_address = self._server_address

        log.debug('Registered CRM %s at %s', name, server_address)

        # Notify relay (outside lock to avoid deadlocks).
        self._relay_register(name, server_address)

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
            proxy = ICRMProxy.thread_local(
                local.crm_instance,
                scheduler=local.scheduler,
                access_map=local.access_map,
            )
        elif address is not None and address.startswith(('http://', 'https://')):
            # HTTP mode — cross-node via relay server.
            client = self._http_pool.acquire(address)
            proxy = ICRMProxy.http(
                client,
                name,
                on_terminate=lambda addr=address: self._http_pool.release(addr),
            )
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

        If ``C2_RELAY_ADDRESS`` is set, the CRM is also unregistered
        from the relay via ``POST /_unregister``.

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

        # Notify relay (outside lock) — best-effort; relay may already be down.
        try:
            self._relay_unregister(name)
        except Exception:
            log.warning(
                'Relay unreachable during unregister of %s — relay may have '
                'shut down already; skipping relay notification.', name,
            )

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

        If ``C2_RELAY_ADDRESS`` is set, all registered CRMs are
        unregistered from the relay before shutting down.

        Called automatically at process exit via :func:`atexit`.
        """
        with self._lock:
            names_to_unregister = list(self._registrations.keys())
            server = self._server
            self._server = None
            self._server_address = None
            self._explicit_address = None
            self._explicit_ipc_config = None
            self._registrations.clear()

        # Best-effort relay unregistration (ignore failures during shutdown).
        for name in names_to_unregister:
            try:
                self._relay_unregister(name)
            except Exception:
                log.info(
                    'Relay unreachable during shutdown unregister of %s — '
                    'relay may have shut down already.', name,
                )

        if server is not None:
            try:
                server.shutdown()
            except Exception:
                log.warning('Error shutting down ServerV2', exc_info=True)

        self._pool.shutdown_all()
        self._http_pool.shutdown_all()

    # ------------------------------------------------------------------
    # Serve (daemon mode)
    # ------------------------------------------------------------------

    _serve_stop: threading.Event | None = None

    def serve(self, blocking: bool = True) -> None:
        """Keep the process alive as a CRM resource server.

        Transitions the process from a computation script into a
        long-running resource service.  Installs signal handlers for
        ``SIGINT`` / ``SIGTERM`` and, when *blocking* is ``True``,
        blocks until a termination signal arrives, then calls
        :meth:`shutdown` for graceful cleanup.

        If no CRM has been registered yet, a warning is logged but
        the process still blocks (useful during development).

        Parameters
        ----------
        blocking:
            If ``True`` (default), block the calling thread until a
            termination signal is received.  If ``False``, install
            signal handlers and print the banner but return immediately
            — the caller is responsible for keeping the process alive.
        """
        if self._serve_stop is not None:
            return  # idempotent

        self._serve_stop = threading.Event()

        with self._lock:
            names = list(self._registrations.keys())
            addr = self._server_address

        if not names:
            log.warning(
                'cc.serve() called with no CRM registered. '
                'The process will block but has nothing to serve.',
            )

        # Print banner.
        self._print_serve_banner(names, addr)

        # Signal handlers — only installable from the main thread.
        def _handle_signal(signum, frame):  # noqa: ARG001
            self._serve_stop.set()

        try:
            signal.signal(signal.SIGINT, _handle_signal)
            if sys.platform != 'win32':
                signal.signal(signal.SIGTERM, _handle_signal)
        except ValueError:
            # Not the main thread — signals cannot be registered.
            # Caller must arrange to call _serve_stop.set() externally.
            log.debug('cc.serve(): signal handlers skipped (not main thread)')

        if not blocking:
            return

        try:
            self._serve_stop.wait()
        except KeyboardInterrupt:
            # On some platforms, SIGINT also raises KeyboardInterrupt
            # even when a custom handler is installed.
            pass

        # Graceful shutdown.
        print('\n Shutting down…')
        self.shutdown()
        self._serve_stop = None
        print(' Resource server stopped.')

    @staticmethod
    def _print_serve_banner(
        names: list[str],
        addr: str | None,
    ) -> None:
        """Print a human-readable startup banner to stdout."""
        lines = [' ── C-Two Resource Server ────────────────']
        if addr:
            lines.append(f'  IPC: {addr}')
        if names:
            lines.append('  CRM routes:')
            for n in names:
                lines.append(f'    • {n}')
        else:
            lines.append('  (no CRM routes registered)')
        lines.append(' ─────────────────────────────────────────')
        count = len(names)
        lines.append(f'  Serving {count} CRM resource(s). Ctrl+C to stop.')
        print('\n'.join(lines))

    # ------------------------------------------------------------------
    # Relay service discovery
    # ------------------------------------------------------------------

    @staticmethod
    def _relay_register(name: str, ipc_address: str) -> None:
        """Notify the relay server about a newly registered CRM.

        Does nothing if ``C2_RELAY_ADDRESS`` is not set.  Raises on
        failure when the env var *is* set (hard error, not warning).
        """
        relay_addr = settings.relay_address
        if not relay_addr:
            return

        url = f'{relay_addr.rstrip("/")}/_register'
        try:
            transport = httpx.HTTPTransport()
            with httpx.Client(transport=transport, timeout=_RELAY_TIMEOUT) as client:
                resp = client.post(url, json={'name': name, 'address': ipc_address})
        except Exception as exc:
            raise ConnectionError(
                f'Failed to register CRM {name!r} with relay at {relay_addr}: {exc}',
            ) from exc

        if resp.status_code not in (200, 201):
            detail = resp.text[:200] if resp.text else resp.reason_phrase
            raise RuntimeError(
                f'Relay rejected registration of {name!r}: '
                f'HTTP {resp.status_code} — {detail}',
            )

        log.info('Registered CRM %s with relay at %s', name, relay_addr)

    @staticmethod
    def _relay_unregister(name: str) -> None:
        """Notify the relay server about a CRM removal.

        Does nothing if ``C2_RELAY_ADDRESS`` is not set.  Raises on
        failure when the env var *is* set (hard error, not warning).
        """
        relay_addr = settings.relay_address
        if not relay_addr:
            return

        url = f'{relay_addr.rstrip("/")}/_unregister'
        try:
            transport = httpx.HTTPTransport()
            with httpx.Client(transport=transport, timeout=_RELAY_TIMEOUT) as client:
                resp = client.post(url, json={'name': name})
        except Exception as exc:
            raise ConnectionError(
                f'Failed to unregister CRM {name!r} from relay at {relay_addr}: {exc}',
            ) from exc

        if resp.status_code not in (200, 204):
            detail = resp.text[:200] if resp.text else resp.reason_phrase
            raise RuntimeError(
                f'Relay rejected unregistration of {name!r}: '
                f'HTTP {resp.status_code} — {detail}',
            )

        log.info('Unregistered CRM %s from relay at %s', name, relay_addr)

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _build_ipc_config(self) -> IPCConfig:
        """Build an :class:`IPCConfig` from the three-level priority chain.

        Priority: ``set_ipc_config()`` > env vars > defaults.
        """
        if self._explicit_ipc_config is not None:
            return self._explicit_ipc_config

        overrides: dict[str, int] = {}
        if settings.ipc_segment_size is not None:
            overrides['pool_segment_size'] = settings.ipc_segment_size
        if settings.ipc_max_segments is not None:
            overrides['max_pool_segments'] = settings.ipc_max_segments

        if overrides:
            return self._make_ipc_config(**overrides)
        return IPCConfig()

    @staticmethod
    def _make_ipc_config(**overrides: int) -> IPCConfig:
        """Create IPCConfig with auto-derived ``max_pool_memory``.

        Raises :class:`ValueError` if ``pool_segment_size`` exceeds
        physical RAM (allocation would fail immediately).  Emits a
        warning if the theoretical pool ceiling (``segment_size ×
        max_segments``) exceeds 75 % of physical RAM.
        """
        seg = overrides.get('pool_segment_size', IPCConfig.pool_segment_size)
        segs = overrides.get('max_pool_segments', IPCConfig.max_pool_segments)
        pool_max = seg * segs
        overrides.setdefault('max_pool_memory', pool_max)

        phys = _physical_memory()
        if phys is not None:
            if seg > phys:
                raise ValueError(
                    f'pool_segment_size ({seg / (1 << 30):.1f} GB) exceeds '
                    f'physical RAM ({phys / (1 << 30):.1f} GB) — '
                    f'SHM allocation will fail'
                )
            if pool_max > int(phys * 0.75):
                log.warning(
                    'Theoretical pool ceiling %.1f GB '
                    '(segment_size=%.1f GB × max_segments=%d) exceeds '
                    '75%% of physical RAM (%.1f GB).  '
                    'Segments are allocated lazily so this may be fine, '
                    'but monitor memory usage under load.',
                    pool_max / (1 << 30),
                    seg / (1 << 30),
                    segs,
                    phys / (1 << 30),
                )

        return IPCConfig(**overrides)

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


def set_ipc_config(
    *,
    segment_size: int | None = None,
    max_segments: int | None = None,
) -> None:
    """Set IPC transport parameters before registering any CRM.

    Priority: ``set_ipc_config()`` > ``C2_IPC_SEGMENT_SIZE`` /
    ``C2_IPC_MAX_SEGMENTS`` env vars > defaults (256 MB / 4 segments).

    See :meth:`_ProcessRegistry.set_ipc_config`.
    """
    _ProcessRegistry.get().set_ipc_config(
        segment_size=segment_size,
        max_segments=max_segments,
    )


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


def serve(blocking: bool = True) -> None:
    """Keep the process alive as a CRM resource server.

    Transitions the calling process from a computation script into a
    long-running resource service.  Blocks until ``SIGINT`` or
    ``SIGTERM``, then calls :func:`shutdown` for graceful cleanup.

    See :meth:`_ProcessRegistry.serve`.
    """
    _ProcessRegistry.get().serve(blocking=blocking)


# Auto-cleanup on process exit.
atexit.register(lambda: _ProcessRegistry.reset())
