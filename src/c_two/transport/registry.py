"""Process-level CRM registry — the SOTA ``cc.register / cc.connect`` API.

Provides a singleton :class:`_ProcessRegistry` that manages:

1. **CRM registration** — registers CRM objects to a process-level
   :class:`Server`, making them accessible via IPC.
2. **Thread preference** — ``connect()`` returns a zero-serialization
   :class:`ICRMProxy.thread_local` when the target CRM lives in the
   same process.
3. **Client pooling** — remote connections reuse Rust IPC/HTTP clients
   via :class:`RustClientPool` and :class:`RustHttpClientPool`.

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
import time
import uuid
from dataclasses import dataclass
from typing import TypeVar

from c_two.config.ipc import ServerIPCConfig, ClientIPCConfig, build_server_config, build_client_config
from c_two.config.settings import settings
from c_two.error import ResourceNotFound, ResourceUnavailable, RegistryUnavailable
from .client.proxy import ICRMProxy
from .server.scheduler import ConcurrencyConfig, Scheduler
from .server.native import NativeServerBridge as Server

from ..crm.meta import MethodAccess

ICRM = TypeVar('ICRM')
log = logging.getLogger(__name__)


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


_ROUTE_CACHE_TTL = 30.0  # seconds


class _RouteCache:
    """Thread-safe TTL cache for resolved routes."""

    def __init__(self):
        self._lock = threading.Lock()
        self._cache: dict[str, tuple[list[dict], float]] = {}

    def get(self, name: str) -> list[dict] | None:
        with self._lock:
            entry = self._cache.get(name)
            if entry and time.monotonic() - entry[1] < _ROUTE_CACHE_TTL:
                return entry[0]
            return None

    def put(self, name: str, routes: list[dict]):
        with self._lock:
            self._cache[name] = (routes, time.monotonic())

    def invalidate(self, name: str):
        with self._lock:
            self._cache.pop(name, None)

    def clear(self):
        with self._lock:
            self._cache.clear()


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
        self._server: Server | None = None
        self._server_address: str | None = None
        self._explicit_address: str | None = None
        self._server_config: ServerIPCConfig | None = None
        self._server_kwargs: dict[str, object] = {}
        self._shm_threshold: int | None = None
        self._client_config: ClientIPCConfig | None = None
        self._client_kwargs: dict[str, object] = {}
        self._pool_config_applied: bool = False
        from c_two._native import RustClientPool, RustHttpClientPool
        self._pool = RustClientPool.instance()
        self._http_pool = RustHttpClientPool.instance()
        self._route_cache = _RouteCache()

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
            IPC address (e.g. ``'ipc://my_server'``).
        """
        with self._lock:
            if self._server is not None:
                raise RuntimeError(
                    'Cannot set address after CRMs have been registered. '
                    'Call set_address() before register().',
                )
            self._explicit_address = address

    def set_relay(self, address: str) -> None:
        """Set the relay address for name resolution.

        Convenience wrapper — equivalent to setting ``C2_RELAY_ADDRESS``.
        """
        settings.relay_address = address

    def set_shm_threshold(self, threshold: int) -> None:
        """Set the SHM threshold globally (server + client).

        Payloads smaller than *threshold* bytes are sent inline;
        larger payloads use shared memory.  Default: 4096 bytes.

        Must be called **before** any :func:`register` or :func:`connect`.

        Parameters
        ----------
        threshold:
            Size in bytes.  Must be > 0.
        """
        if threshold <= 0:
            raise ValueError(f'shm_threshold must be > 0, got {threshold}')
        with self._lock:
            if self._server is not None:
                raise RuntimeError(
                    'Cannot set shm_threshold after CRMs have been registered. '
                    'Call set_shm_threshold() before register().',
                )
            self._shm_threshold = threshold

    def set_config(self, *, shm_threshold: int | None = None) -> None:
        """Set global config. Must be called before register()/connect()."""
        with self._lock:
            if self._server is not None or self._pool_config_applied:
                import warnings
                warnings.warn(
                    'Active connections exist, set_config() ignored. '
                    'Call set_config() before register()/connect().',
                    UserWarning,
                    stacklevel=3,
                )
                return
            if shm_threshold is not None:
                if shm_threshold <= 0:
                    raise ValueError(f'shm_threshold must be > 0, got {shm_threshold}')
                self._shm_threshold = shm_threshold

    def set_server(self, **kwargs: object) -> None:
        """Configure IPC server. Must be called before register()."""
        with self._lock:
            if self._server is not None:
                import warnings
                warnings.warn(
                    'Server already started, set_server() ignored. '
                    'Call set_server() before register().',
                    UserWarning,
                    stacklevel=3,
                )
                return
            self._server_kwargs = kwargs
            self._server_config = None  # rebuilt lazily

    def set_client(self, **kwargs: object) -> None:
        """Configure IPC client. Must be called before connect()."""
        with self._lock:
            if self._pool_config_applied:
                import warnings
                warnings.warn(
                    'Client connections already exist, set_client() ignored. '
                    'Call set_client() before connect().',
                    UserWarning,
                    stacklevel=3,
                )
                return
            self._client_kwargs = kwargs
            self._client_config = None  # rebuilt lazily

    def register(
        self,
        icrm_class: type,
        crm_instance: object,
        *,
        name: str,
        concurrency: ConcurrencyConfig | None = None,
    ) -> str:
        """Register a CRM, making it available via :func:`connect`.

        On the first call, a :class:`Server` is created and started
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
                server_cfg = self._build_server_config()
                shm = self._shm_threshold or settings.shm_threshold or 4096
                self._server = Server(bind_address=addr, ipc_config=server_cfg, shm_threshold=shm)
                self._server_address = addr
                # Rust pool uses its own defaults; skip Python IPCConfig.

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
            Explicit IPC server address (e.g. ``'ipc://remote'``).
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
            # Remote IPC via pooled RustClient.
            if not self._pool_config_applied:
                cfg = self._build_client_config()
                shm = self._shm_threshold or settings.shm_threshold or 4096
                self._pool.set_default_config(
                    shm_threshold=shm,
                    pool_enabled=cfg.pool_enabled,
                    pool_segment_size=cfg.pool_segment_size,
                    max_pool_segments=cfg.max_pool_segments,
                    max_pool_memory=cfg.max_pool_memory,
                    reassembly_segment_size=cfg.reassembly_segment_size,
                    reassembly_max_segments=cfg.reassembly_max_segments,
                    max_total_chunks=cfg.max_total_chunks,
                    chunk_gc_interval=cfg.chunk_gc_interval,
                    chunk_threshold_ratio=cfg.chunk_threshold_ratio,
                    chunk_assembler_timeout=cfg.chunk_assembler_timeout,
                    max_reassembly_bytes=cfg.max_reassembly_bytes,
                    chunk_size=cfg.chunk_size,
                )
                self._pool_config_applied = True
            client = self._pool.acquire(address)
            proxy = ICRMProxy.ipc(
                client,
                name,
                on_terminate=lambda addr=address: self._pool.release(addr),
            )
        else:
            # 3. Relay resolution — no local CRM and no explicit address.
            try:
                routes = self._resolve_via_relay(name)
            except RegistryUnavailable:
                raise LookupError(
                    f'Name {name!r} is not registered locally '
                    f'and no address was provided',
                )
            except (ResourceNotFound, ResourceUnavailable):
                raise
            except Exception:
                raise LookupError(
                    f'Name {name!r} is not registered locally '
                    f'and relay resolution failed',
                )

            if not routes:
                raise ResourceNotFound(f"No routes for '{name}'")

            # Try each route in priority order (LOCAL first, then PEER).
            last_error = None
            for route in routes:
                try:
                    ipc_addr = route.get("ipc_address")
                    if ipc_addr:
                        address = ipc_addr
                    else:
                        relay_url = route.get("relay_url", "")
                        address = f"{relay_url}/{name}"

                    if address.startswith(('http://', 'https://')):
                        client = self._http_pool.acquire(address)
                        proxy = ICRMProxy.http(
                            client,
                            name,
                            on_terminate=lambda addr=address: self._http_pool.release(addr),
                        )
                    else:
                        if not self._pool_config_applied:
                            cfg = self._build_client_config()
                            shm = self._shm_threshold or settings.shm_threshold or 4096
                            self._pool.set_default_config(
                                shm_threshold=shm,
                                pool_enabled=cfg.pool_enabled,
                                pool_segment_size=cfg.pool_segment_size,
                                max_pool_segments=cfg.max_pool_segments,
                                max_pool_memory=cfg.max_pool_memory,
                                reassembly_segment_size=cfg.reassembly_segment_size,
                                reassembly_max_segments=cfg.reassembly_max_segments,
                                max_total_chunks=cfg.max_total_chunks,
                                chunk_gc_interval=cfg.chunk_gc_interval,
                                chunk_threshold_ratio=cfg.chunk_threshold_ratio,
                                chunk_assembler_timeout=cfg.chunk_assembler_timeout,
                                max_reassembly_bytes=cfg.max_reassembly_bytes,
                                chunk_size=cfg.chunk_size,
                            )
                            self._pool_config_applied = True
                        client = self._pool.acquire(address)
                        proxy = ICRMProxy.ipc(
                            client,
                            name,
                            on_terminate=lambda addr=address: self._pool.release(addr),
                        )

                    icrm = icrm_class()
                    icrm.client = proxy
                    return icrm
                except Exception as e:
                    last_error = e
                    self._route_cache.invalidate(name)
                    continue

            raise ResourceUnavailable(
                f"All routes for '{name}' unreachable: {last_error}",
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
            self._server_config = None
            self._server_kwargs = {}
            self._shm_threshold = None
            self._client_config = None
            self._client_kwargs = {}
            self._pool_config_applied = False
            self._registrations.clear()

        self._route_cache.clear()

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
                log.warning('Error shutting down Server', exc_info=True)

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

        # Signal handlers — install BEFORE banner so that SIGINT is
        # handled correctly as soon as the caller sees the output.
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

        # Print banner (after signal handlers are ready).
        self._print_serve_banner(names, addr)

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
    def _relay_register(name: str, ipc_address: str, *, timeout: float = 5.0) -> None:
        """Notify the relay server about a newly registered CRM.

        Does nothing if ``C2_RELAY_ADDRESS`` is not set.  Raises on
        failure when the env var *is* set (hard error, not warning).
        """
        import json
        import urllib.request

        relay_addr = settings.relay_address
        if not relay_addr:
            return

        url = f'{relay_addr.rstrip("/")}/_register'
        body = json.dumps({'name': name, 'address': ipc_address}).encode()
        req = urllib.request.Request(
            url, data=body,
            headers={'Content-Type': 'application/json'},
            method='POST',
        )
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                status = resp.status
        except Exception as exc:
            raise ConnectionError(
                f'Failed to register CRM {name!r} with relay at {relay_addr}: {exc}',
            ) from exc

        if status not in (200, 201):
            raise RuntimeError(
                f'Relay rejected registration of {name!r}: HTTP {status}',
            )

        log.info('Registered CRM %s with relay at %s', name, relay_addr)

    @staticmethod
    def _relay_unregister(name: str, *, timeout: float = 5.0) -> None:
        """Notify the relay server about a CRM removal.

        Does nothing if ``C2_RELAY_ADDRESS`` is not set.  Raises on
        failure when the env var *is* set (hard error, not warning).
        """
        import json
        import urllib.request

        relay_addr = settings.relay_address
        if not relay_addr:
            return

        url = f'{relay_addr.rstrip("/")}/_unregister'
        body = json.dumps({'name': name}).encode()
        req = urllib.request.Request(
            url, data=body,
            headers={'Content-Type': 'application/json'},
            method='POST',
        )
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                status = resp.status
        except Exception as exc:
            raise ConnectionError(
                f'Failed to unregister CRM {name!r} from relay at {relay_addr}: {exc}',
            ) from exc

        if status not in (200, 204):
            raise RuntimeError(
                f'Relay rejected unregistration of {name!r}: HTTP {status}',
            )

        log.info('Unregistered CRM %s from relay at %s', name, relay_addr)

    def _resolve_via_relay(self, name: str) -> list[dict]:
        """Resolve a resource name via the relay's /_resolve endpoint."""
        cached = self._route_cache.get(name)
        if cached is not None:
            return cached

        relay_addr = settings.relay_address
        if not relay_addr:
            raise RegistryUnavailable("No relay configured (set C2_RELAY_ADDRESS or call cc.set_relay())")

        import json
        import urllib.request

        url = f'{relay_addr.rstrip("/")}/_resolve/{name}'
        try:
            req = urllib.request.Request(url)
            with urllib.request.urlopen(req, timeout=5) as resp:
                routes = json.loads(resp.read())
                self._route_cache.put(name, routes)
                return routes
        except urllib.error.HTTPError as e:
            if e.code == 404:
                raise ResourceNotFound(f"Resource '{name}' not found") from e
            raise ResourceUnavailable(f"Relay error: {e.code}") from e
        except Exception as e:
            self._route_cache.invalidate(name)
            raise RegistryUnavailable(f"Relay unreachable: {e}") from e

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _build_server_config(self) -> ServerIPCConfig:
        if self._server_config is not None:
            return self._server_config
        self._server_config = build_server_config(settings, **self._server_kwargs)
        return self._server_config

    def _build_client_config(self) -> ClientIPCConfig:
        if self._client_config is not None:
            return self._client_config
        self._client_config = build_client_config(settings, **self._client_kwargs)
        return self._client_config

    @staticmethod
    def _auto_address() -> str:
        return f'ipc://cc_auto_{os.getpid()}_{uuid.uuid4().hex[:8]}'


# ------------------------------------------------------------------
# Module-level API (delegates to singleton)
# ------------------------------------------------------------------

def set_ipc_address(address: str) -> None:
    """Set the IPC server address before registering any CRM.

    Priority: ``set_address()`` > ``C2_IPC_ADDRESS`` env var > auto.

    See :meth:`_ProcessRegistry.set_address`.
    """
    _ProcessRegistry.get().set_address(address)


def set_config(*, shm_threshold: int | None = None) -> None:
    """Set global hardware config. Call before register()/connect()."""
    _ProcessRegistry.get().set_config(shm_threshold=shm_threshold)


def set_server(**kwargs: object) -> None:
    """Configure IPC server. Call before register()."""
    _ProcessRegistry.get().set_server(**kwargs)


def set_client(**kwargs: object) -> None:
    """Configure IPC client. Call before connect()."""
    _ProcessRegistry.get().set_client(**kwargs)


def set_relay(address: str) -> None:
    """Set the relay address for name resolution."""
    _ProcessRegistry.get().set_relay(address)


# Deprecated aliases — remove in next version
def set_shm_threshold(threshold: int) -> None:
    """Deprecated: use set_config(shm_threshold=...) instead."""
    set_config(shm_threshold=threshold)


def set_server_ipc_config(**kw: object) -> None:
    """Deprecated: use set_server() instead."""
    set_server(**kw)


def set_client_ipc_config(**kw: object) -> None:
    """Deprecated: use set_client() instead."""
    set_client(**kw)


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


def hold_stats() -> dict:
    """Return hold-mode SHM tracking statistics.

    Returns dict with:
    - active_holds: number of currently held SHM buffers
    - total_held_bytes: total bytes pinned in SHM
    - oldest_hold_seconds: age of oldest active hold
    """
    inst = _ProcessRegistry._instance
    server = inst._server if inst else None
    if server is None:
        return {'active_holds': 0, 'total_held_bytes': 0, 'oldest_hold_seconds': 0}
    return server.hold_stats()


# Auto-cleanup on process exit.
atexit.register(lambda: _ProcessRegistry.reset())
