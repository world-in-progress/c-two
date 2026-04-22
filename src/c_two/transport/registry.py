"""Process-level CRM registry — the SOTA ``cc.register / cc.connect`` API.

Provides a singleton :class:`_ProcessRegistry` that manages:

1. **CRM registration** — registers CRM objects to a process-level
   :class:`Server`, making them accessible via IPC.
2. **Thread preference** — ``connect()`` returns a zero-serialization
   :class:`CRMProxy.thread_local` when the target CRM lives in the
   same process.
3. **Client pooling** — remote connections reuse Rust IPC/HTTP clients
   via :class:`RustClientPool` and :class:`RustHttpClientPool`.

Usage::

    import c_two as cc

    # Register CRMs with explicit names
    cc.register(Grid, grid_instance, name='grid')

    # Connect (same process → thread-local, no serialization)
    grid = cc.connect(Grid, name='grid')
    result = grid.subdivide_grids([1], [0])

    # Close the connection
    cc.close(grid)

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
import os
import threading
import time
import urllib.request
import uuid
from dataclasses import dataclass
from urllib.parse import quote as _urlquote
from urllib.parse import urlparse as _urlparse
from typing import TypeVar


# Build once: an opener that BYPASSES system HTTP_PROXY for all relay traffic.
# Rationale: the c-two relay is private mesh infrastructure on user-controlled
# hosts. Routing relay traffic through a corporate / dev HTTP proxy causes
# subtle bugs (e.g., proxies normalize percent-encoded `%2F` in path segments
# to `/`, breaking resource names that contain `/`). It is also a privacy /
# correctness hazard to leak internal mesh control traffic to a third-party
# proxy. An empty ProxyHandler({}) tells urllib to never use any proxy.
#
# Users who actually want to route relay traffic through the system proxy
# (e.g., behind a forward proxy that doesn't normalize URLs) can opt in by
# setting C2_RELAY_USE_PROXY=1.
_NO_PROXY_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def _relay_use_proxy() -> bool:
    """Read C2_RELAY_USE_PROXY directly from the environment.

    We read the env var at call time (not via the cached pydantic settings
    object) so behavior matches the Rust side, which also reads the env on
    every client creation. This avoids a confusing "Python and Rust
    disagree" failure mode if the user mutates the env after import.
    """
    val = os.environ.get("C2_RELAY_USE_PROXY", "").strip().lower()
    return val in ("1", "true", "yes")


def _relay_urlopen(req, timeout: float = 5.0):
    """urlopen for relay traffic. Honors C2_RELAY_USE_PROXY (default: bypass)."""
    if _relay_use_proxy():
        return urllib.request.urlopen(req, timeout=timeout)
    return _NO_PROXY_OPENER.open(req, timeout=timeout)

from c_two.config.ipc import ServerIPCConfig, ClientIPCConfig, build_server_config, build_client_config
from c_two.config.settings import settings
from c_two.error import ResourceNotFound, ResourceUnavailable, RegistryUnavailable
from .client.proxy import CRMProxy
from .server.scheduler import ConcurrencyConfig, Scheduler
from .server.native import NativeServerBridge as Server

from ..crm.meta import MethodAccess

CRM = TypeVar('CRM')
log = logging.getLogger(__name__)


_LOOPBACK_HOSTS = frozenset({
    'localhost',
    '127.0.0.1',
    '::1',
    '0.0.0.0',
})


def _is_local_relay_url(relay_url: str) -> bool:
    """Return True if ``relay_url`` points at a relay on the same host.

    Used to decide whether a route's ``ipc_address`` (a local UDS path on
    the relay's filesystem) is reachable from this client. When the relay
    sits at a non-loopback address it must be a remote machine, and the
    UDS path is meaningless to us.
    """
    if not relay_url:
        return False
    try:
        host = (_urlparse(relay_url).hostname or '').lower()
    except Exception:
        return False
    return host in _LOOPBACK_HOSTS


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
    crm_class: type
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
        self._server_config: ServerIPCConfig | None = None
        self._bg_cancel = threading.Event()
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

    def set_relay(self, address: str) -> None:
        """Set the relay address for name resolution.

        Convenience wrapper — equivalent to setting ``C2_RELAY_ADDRESS``.
        """
        settings.relay_address = address

    def set_shm_threshold(self, threshold: int) -> None:
        """Set the SHM threshold globally (server + client).

        Thin wrapper over :meth:`set_config` kept for internal callers that
        configure a single field.  Must be called **before** any
        :func:`register` or :func:`connect`.

        Parameters
        ----------
        threshold:
            Size in bytes.  Must be > 0.
        """
        if threshold <= 0:
            raise ValueError(f'shm_threshold must be > 0, got {threshold}')
        self.set_config(shm_threshold=threshold)

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
        crm_class: type,
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
        crm_class:
            ``@cc.crm``-decorated interface (contract) class.
        crm_instance:
            Concrete CRM object that implements *crm_class*.
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
                addr = self._auto_address()
                server_cfg = self._build_server_config()
                shm = self._shm_threshold or settings.shm_threshold or 4096
                self._server = Server(bind_address=addr, ipc_config=server_cfg, shm_threshold=shm)
                self._server_address = addr
                # Rust pool uses its own defaults; skip Python IPCConfig.

            self._server.register_crm(crm_class, crm_instance, concurrency, name=name)
            scheduler, access_map = self._server.get_slot_info(name)
            self._registrations[name] = _Registration(
                name=name,
                crm_class=crm_class,
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
        crm_ns = getattr(crm_class, '__cc_namespace__', '')
        crm_ver = getattr(crm_class, '__cc_version__', '')
        self._relay_register(name, server_address, crm_ns, crm_ver)

        return name

    def connect(
        self,
        crm_class: type[CRM],
        *,
        name: str,
        address: str | None = None,
    ) -> CRM:
        """Obtain a CRM instance connected to a registered resource.

        **Thread preference**: when the target *name* is registered in
        this process and no explicit *address* is given, returns a
        zero-serialization local proxy.

        Parameters
        ----------
        crm_class:
            ``@cc.crm``-decorated interface (contract) class.
        name:
            Routing name of the target CRM.
        address:
            Explicit IPC server address (e.g. ``'ipc://remote'``).
            If ``None``, looks up the local registry.

        Returns
        -------
        object
            A CRM instance with ``.client`` set to an
            :class:`CRMProxy`.
        """
        with self._lock:
            local = self._registrations.get(name)

        if address is None and local is not None:
            # Thread preference — same process, no serialization.
            proxy = CRMProxy.thread_local(
                local.crm_instance,
                scheduler=local.scheduler,
                access_map=local.access_map,
            )
        elif address is not None and address.startswith(('http://', 'https://')):
            # HTTP mode — cross-node via relay server.
            client = self._http_pool.acquire(address)
            proxy = CRMProxy.http(
                client,
                name,
                on_terminate=lambda addr=address: self._http_pool.release(addr),
            )
        elif address is not None:
            # Remote IPC via pooled RustClient.
            self._ensure_pool_config()
            client = self._pool.acquire(address)
            proxy = CRMProxy.ipc(
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
                    relay_url = route.get("relay_url", "") or ""
                    ipc_addr = route.get("ipc_address")
                    # ipc_address is a UDS path on the relay's filesystem —
                    # only usable when this client is on the same host as the
                    # owning relay. Detect that via loopback / empty relay_url
                    # (i.e. it's our local relay reporting one of its own
                    # routes). Otherwise we MUST go through the relay's HTTP
                    # endpoint instead.
                    if ipc_addr and _is_local_relay_url(relay_url):
                        address = ipc_addr
                    elif relay_url:
                        # HTTP base URL is just the relay; the Rust HTTP
                        # client appends `/{route_name}/{method_name}` on
                        # each call.
                        address = relay_url
                    elif ipc_addr:
                        # Last resort: no relay_url known, fall back to
                        # ipc_addr (legacy behaviour).
                        address = ipc_addr
                    else:
                        last_error = ResourceUnavailable(
                            f"Route for {name!r} has neither relay_url nor ipc_address",
                        )
                        continue

                    if address.startswith(('http://', 'https://')):
                        client = self._http_pool.acquire(address)
                        proxy = CRMProxy.http(
                            client,
                            name,
                            on_terminate=lambda addr=address: self._http_pool.release(addr),
                        )
                    else:
                        self._ensure_pool_config()
                        client = self._pool.acquire(address)
                        proxy = CRMProxy.ipc(
                            client,
                            name,
                            on_terminate=lambda addr=address: self._pool.release(addr),
                        )

                    crm = crm_class()
                    crm.client = proxy
                    return crm
                except Exception as e:
                    last_error = e
                    self._route_cache.invalidate(name)
                    continue

            raise ResourceUnavailable(
                f"All routes for '{name}' unreachable: {last_error}",
            )

        crm = crm_class()
        crm.client = proxy
        return crm

    def close(self, crm: object) -> None:
        """Close a connection obtained from :func:`connect`.

        Terminates the underlying proxy and releases pool references.
        """
        proxy = getattr(crm, 'client', None)
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
            self._server_config = None
            self._server_kwargs = {}
            self._shm_threshold = None
            self._client_config = None
            self._client_kwargs = {}
            self._pool_config_applied = False
            self._registrations.clear()

        self._bg_cancel.set()
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
        self._bg_cancel = threading.Event()  # Reset for potential reuse

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

    def _relay_register(self, name: str, ipc_address: str,
                        crm_ns: str = '', crm_ver: str = ''):
        """Notify relay about a new CRM registration with retry."""
        relay_addr = settings.relay_address
        if not relay_addr:
            return  # No relay configured — standalone mode.

        import json

        payload = json.dumps({
            "name": name,
            "address": ipc_address,
            "crm_ns": crm_ns,
            "crm_ver": crm_ver,
        }).encode()

        url = f'{relay_addr.rstrip("/")}/_register'
        for attempt in range(3):
            try:
                req = urllib.request.Request(
                    url, data=payload,
                    headers={"Content-Type": "application/json"},
                    method="POST",
                )
                with _relay_urlopen(req, timeout=5) as resp:
                    if resp.status in (200, 201):
                        log.info('Registered CRM %s with relay at %s',
                                 name, relay_addr)
                        return
            except Exception:
                if attempt < 2:
                    time.sleep(1)

        # All retries failed → register locally, schedule background retry.
        log.warning('Relay unreachable for %s — scheduling background retry',
                    name)
        self._schedule_background_relay_register(
            name, ipc_address, crm_ns, crm_ver)

    def _schedule_background_relay_register(self, name, ipc_address,
                                            crm_ns, crm_ver):
        """Background thread that keeps trying to register with relay."""
        import json

        def retry_loop():
            relay_addr = settings.relay_address
            if not relay_addr:
                return
            payload = json.dumps({
                "name": name, "address": ipc_address,
                "crm_ns": crm_ns, "crm_ver": crm_ver,
            }).encode()
            url = f'{relay_addr.rstrip("/")}/_register'
            delay = 1.0
            max_delay = 60.0
            while not self._bg_cancel.is_set():
                self._bg_cancel.wait(delay)
                if self._bg_cancel.is_set():
                    return
                try:
                    req = urllib.request.Request(
                        url, data=payload,
                        headers={"Content-Type": "application/json"},
                        method="POST",
                    )
                    with _relay_urlopen(req, timeout=5):
                        log.info('Background relay register succeeded for %s',
                                 name)
                        return
                except Exception:
                    delay = min(delay * 2, max_delay)

        t = threading.Thread(target=retry_loop, daemon=True)
        t.start()

    def _relay_unregister(self, name: str, *, timeout: float = 5.0) -> None:
        """Notify relay about CRM unregistration with retry."""
        relay_addr = settings.relay_address
        if not relay_addr:
            return

        import json

        url = f'{relay_addr.rstrip("/")}/_unregister'
        body = json.dumps({'name': name}).encode()
        for attempt in range(3):
            try:
                req = urllib.request.Request(
                    url, data=body,
                    headers={'Content-Type': 'application/json'},
                    method='POST',
                )
                with _relay_urlopen(req, timeout=timeout) as resp:
                    if resp.status in (200, 204):
                        log.info('Unregistered CRM %s from relay', name)
                        return
            except Exception:
                if attempt < 2:
                    time.sleep(1)
        # Background retry not needed for unregister — route will be cleaned
        # by anti-entropy and failure detection.
        log.warning('Failed to notify relay about unregistration of %s', name)

    def _resolve_via_relay(self, name: str) -> list[dict]:
        """Resolve a resource name via the relay's /_resolve endpoint."""
        cached = self._route_cache.get(name)
        if cached is not None:
            return cached

        relay_addr = settings.relay_address
        if not relay_addr:
            raise RegistryUnavailable("No relay configured (set C2_RELAY_ADDRESS or call cc.set_relay())")

        import json

        url = f'{relay_addr.rstrip("/")}/_resolve/{_urlquote(name, safe="")}'
        try:
            req = urllib.request.Request(url)
            with _relay_urlopen(req, timeout=5) as resp:
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

    def _ensure_pool_config(self) -> None:
        """Apply pool default config exactly once (thread-safe)."""
        with self._lock:
            if self._pool_config_applied:
                return
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

    @staticmethod
    def _auto_address() -> str:
        return f'ipc://cc_auto_{os.getpid()}_{uuid.uuid4().hex[:8]}'


# ------------------------------------------------------------------
# Module-level API (delegates to singleton)
# ------------------------------------------------------------------

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


def register(
    crm_class: type,
    crm_instance: object,
    *,
    name: str,
    concurrency: ConcurrencyConfig | None = None,
) -> str:
    """Register a CRM in the current process.

    See :meth:`_ProcessRegistry.register`.
    """
    return _ProcessRegistry.get().register(
        crm_class,
        crm_instance,
        name=name,
        concurrency=concurrency,
    )


def connect(
    crm_class: type[CRM],
    *,
    name: str,
    address: str | None = None,
) -> CRM:
    """Obtain a CRM proxy for a registered resource.

    See :meth:`_ProcessRegistry.connect`.
    """
    return _ProcessRegistry.get().connect(crm_class, name=name, address=address)


def close(crm: object) -> None:
    """Close a connection obtained from :func:`connect`.

    See :meth:`_ProcessRegistry.close`.
    """
    _ProcessRegistry.get().close(crm)


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
