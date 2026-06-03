"""Process-level CRM registry — the SOTA ``cc.register / cc.connect`` API.

Provides a singleton :class:`_ProcessRegistry` that manages:

1. **CRM registration** — registers CRM objects to a process-level
   :class:`Server`, making them accessible via IPC.
2. **Thread preference** — ``connect()`` returns a zero-serialization
   :class:`CRMProxy.thread_local` when the target CRM lives in the
   same process.
3. **Client pooling** — remote connections reuse Rust-owned IPC/HTTP client
   pools through the native runtime session.

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
import signal
import sys
import threading
from collections.abc import Mapping
from typing import TypeVar

from c_two.crm.bridge import ResourceBridge, normalize_bridge_map
from c_two.crm.contract import CRMContract, crm_contract, crm_contract_identity
from c_two.crm.conformance import validate_resource_conformance
from c_two.crm.methods import rpc_method_names
from c_two.config.ipc import ClientIPCOverrides, ServerIPCOverrides
from c_two.config.settings import settings
from c_two.error import (
    ResourceNotFound,
    ResourceUnavailable,
    RegistryUnavailable,
)
from .client.proxy import CRMProxy
from .input_lifetime import (
    InputLifetimeLike,
    normalize_input_lifetime_map,
    validate_input_lifetime_resource_contract,
)
from .server.scheduler import ConcurrencyConfig
from .server.native import NativeServerBridge as Server

CRM = TypeVar('CRM')
log = logging.getLogger(__name__)
_UNSET = object()


def _runtime_session_kwargs_from_settings() -> dict[str, int]:
    kwargs: dict[str, int] = {}
    shm_overrides = settings._shm_overrides()  # noqa: SLF001
    if 'shm_threshold' in shm_overrides:
        kwargs['shm_threshold'] = shm_overrides['shm_threshold']
    remote_chunk_size = settings._remote_payload_chunk_size_override()  # noqa: SLF001
    if remote_chunk_size is not None:
        kwargs['remote_payload_chunk_size'] = remote_chunk_size
    return kwargs


def _relay_control_error_status(exc: BaseException) -> int | None:
    value = getattr(exc, "status_code", None)
    return value if isinstance(value, int) else None


def _is_crm_contract_mismatch(exc: BaseException) -> bool:
    return isinstance(exc, RuntimeError) and "CRM contract mismatch" in str(exc)


def _ensure_crm_contract_match(
    *,
    route_name: str,
    expected: CRMContract,
    actual: CRMContract,
) -> None:
    if expected == actual:
        return
    raise TypeError(
        f'CRM contract mismatch for route {route_name!r}: '
        f'expected {expected.crm_ns}/{expected.crm_name}/{expected.crm_ver} '
        f'abi={expected.abi_hash} signature={expected.signature_hash}, '
        f'got {actual.crm_ns}/{actual.crm_name}/{actual.crm_ver} '
        f'abi={actual.abi_hash} signature={actual.signature_hash}',
    )


def _relay_cleanup_exception(error: dict[str, object]) -> RuntimeError:
    route_name = str(error.get('route_name') or '<unknown>')
    message = str(error.get('message') or 'unknown relay cleanup error')
    status_code = error.get('status_code')
    if isinstance(status_code, int):
        return RuntimeError(
            f'Relay unregistration failed for {route_name!r}: '
            f'HTTP {status_code}: {message}',
        )
    return RuntimeError(
        f'Relay unregistration failed for {route_name!r}: {message}',
    )


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
        from c_two._native import RuntimeSession
        self._server: Server | None = None
        self._runtime_session = RuntimeSession(**_runtime_session_kwargs_from_settings())

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def set_relay_anchor(self, address: str) -> None:
        """Set the relay anchor address for name resolution.

        Convenience wrapper — equivalent to setting ``C2_RELAY_ANCHOR_ADDRESS``.
        """
        settings.relay_anchor_address = address
        self._runtime_session.set_relay_anchor_address(settings._relay_anchor_address)  # noqa: SLF001

    def set_transport_policy(
        self,
        *,
        shm_threshold: int | None | object = _UNSET,
        remote_payload_chunk_size: int | None | object = _UNSET,
    ) -> None:
        """Set process transport policy. Must be called before use."""
        with self._lock:
            if self._server is not None or self._runtime_session.client_config_frozen:
                import warnings
                warnings.warn(
                    'Active connections exist, set_transport_policy() ignored. '
                    'Call set_transport_policy() before register()/connect().',
                    UserWarning,
                    stacklevel=3,
                )
                return
            if shm_threshold is not _UNSET:
                settings.shm_threshold = shm_threshold  # type: ignore[assignment]
            if remote_payload_chunk_size is not _UNSET:
                settings.remote_payload_chunk_size = remote_payload_chunk_size  # type: ignore[assignment]
            runtime_session_kwargs = _runtime_session_kwargs_from_settings()
            runtime_session = self._runtime_session.__class__(
                server_id=(
                    self._runtime_session.server_id
                    or self._runtime_session.server_id_override
                ),
                server_ipc_overrides=self._runtime_session.server_ipc_overrides,
                client_ipc_overrides=self._runtime_session.client_ipc_overrides,
                **runtime_session_kwargs,
            )
            runtime_session.set_relay_anchor_address(settings._relay_anchor_address)  # noqa: SLF001
            self._runtime_session = runtime_session

    def set_server(
        self,
        *,
        server_id: str | None = None,
        ipc_overrides: ServerIPCOverrides | Mapping[str, object] | None = None,
    ) -> None:
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
            self._runtime_session.set_server_options(server_id, ipc_overrides)

    def set_client(
        self,
        *,
        ipc_overrides: ClientIPCOverrides | Mapping[str, object] | None = None,
    ) -> None:
        """Configure IPC client. Must be called before connect()."""
        with self._lock:
            if self._runtime_session.client_config_frozen:
                import warnings
                warnings.warn(
                    'Client connections already exist, set_client() ignored. '
                    'Call set_client() before connect().',
                    UserWarning,
                    stacklevel=3,
                )
                return
            if not self._runtime_session.set_client_ipc_overrides(ipc_overrides):
                import warnings
                warnings.warn(
                    'Client connections already exist, set_client() ignored. '
                    'Call set_client() before connect().',
                    UserWarning,
                    stacklevel=3,
                )

    def register(
        self,
        crm_class: type,
        crm_instance: object,
        *,
        name: str,
        concurrency: ConcurrencyConfig | None = None,
        bridge: dict[str, ResourceBridge] | None = None,
        input_lifetime: Mapping[str, InputLifetimeLike] | None = None,
    ) -> str:
        """Register a CRM, making it available via :func:`connect`.

        On the first call, a :class:`Server` is created and started
        automatically (lazy init).

        If ``C2_RELAY_ANCHOR_ADDRESS`` is set, the CRM is also registered with
        the relay server through the Rust relay control client. A
        connection failure or non-2xx response raises an error.

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
        crm_contract_identity(crm_class)
        method_names = rpc_method_names(crm_class)
        method_bridge_map = normalize_bridge_map(
            bridge,
            method_names=method_names,
        )
        input_lifetime_map = normalize_input_lifetime_map(
            input_lifetime,
            method_names=method_names,
        )
        validate_resource_conformance(
            crm_class,
            crm_instance,
            bridge=method_bridge_map,
        )
        validate_input_lifetime_resource_contract(
            crm_class,
            crm_instance,
            input_lifetime_map,
            bridge=method_bridge_map,
        )
        with self._lock:
            created_server = False
            try:
                self._sync_relay_override()
                # Lazy-init server on first registration. Native identity is
                # cleared if Python server bridge construction fails, so invalid
                # server config does not leave a visible server id/address.
                server = self._server
                if server is None:
                    try:
                        server = self._runtime_session.ensure_server_bridge()
                    except Exception:
                        self._runtime_session.clear_server_identity()
                        raise
                    self._server = server
                    created_server = True

                server.register_crm(
                    crm_class,
                    crm_instance,
                    concurrency,
                    name=name,
                    bridge=method_bridge_map,
                    input_lifetime=input_lifetime_map,
                    runtime_session=self._runtime_session,
                    relay_anchor_address=None,
                )

                # Start server if not yet running.
                if not server.is_started():
                    server.start()

                server_address = self._runtime_session.server_address
            except Exception:
                if created_server:
                    server = self._server
                    self._server = None
                    self._runtime_session.clear_server_identity()
                    if server is not None:
                        try:
                            server.shutdown()
                        except Exception:
                            log.warning('Error shutting down Server after failed register', exc_info=True)
                raise

        log.debug('Registered CRM %s at %s', name, server_address)

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
        expected_contract = crm_contract(crm_class)
        with self._lock:
            server = self._server
            local = (
                server.get_local_slot_info(name)
                if address is None and server is not None
                else None
            )
            lease_tracker = self._runtime_session.lease_tracker()

        if address is None and local is not None:
            # Thread preference — same process, no serialization.
            crm_instance, scheduler, actual_contract = local
            _ensure_crm_contract_match(
                route_name=name,
                expected=expected_contract,
                actual=actual_contract,
            )
            proxy = CRMProxy.thread_local(
                crm_instance,
                scheduler=scheduler,
                lease_tracker=lease_tracker,
            )
        elif address is not None and address.startswith(('http://', 'https://')):
            # HTTP mode — cross-node via relay server.
            try:
                client = self._runtime_session.connect_explicit_relay_http(
                    address,
                    name,
                    *expected_contract.native_args(),
                )
            except Exception as exc:
                if _is_crm_contract_mismatch(exc):
                    raise
                status = _relay_control_error_status(exc)
                if status == 404:
                    raise ResourceNotFound(f"Resource '{name}' not found") from exc
                if status is not None:
                    raise ResourceUnavailable(f"Relay error: {status}") from exc
                raise
            proxy = CRMProxy.http(
                client,
                name,
                on_terminate=client.close,
                lease_tracker=lease_tracker,
            )
        elif address is not None:
            # Remote IPC via pooled RustClient.
            client = self._runtime_session.acquire_ipc_client(
                address,
                name,
                *expected_contract.native_args(),
            )
            proxy = CRMProxy.ipc(
                client,
                name,
                on_terminate=lambda addr=address: self._runtime_session.release_ipc_client(addr),
                lease_tracker=lease_tracker,
            )
        else:
            self._sync_relay_override()
            try:
                client = self._runtime_session.connect_via_relay(
                    name,
                    *expected_contract.native_args(),
                )
            except Exception as exc:
                if _is_crm_contract_mismatch(exc):
                    raise
                status = _relay_control_error_status(exc)
                if status == 404:
                    raise ResourceNotFound(f"Resource '{name}' not found") from exc
                if status is not None:
                    raise ResourceUnavailable(f"Relay error: {status}") from exc
                if isinstance(exc, LookupError):
                    raise LookupError(
                        f'Name {name!r} is not registered locally '
                        f'and no address was provided',
                    ) from exc
                raise RegistryUnavailable(f"Relay unavailable: {exc}") from exc
            mode = getattr(client, 'mode', 'http')
            if mode == 'ipc':
                proxy = CRMProxy.ipc(
                    client,
                    name,
                    on_terminate=lambda: client.close(),
                    lease_tracker=lease_tracker,
                )
            elif mode == 'http':
                proxy = CRMProxy.http(
                    client,
                    name,
                    on_terminate=lambda: client.close(),
                    lease_tracker=lease_tracker,
                )
            else:
                raise RegistryUnavailable(f"Unsupported relay target mode: {mode!r}")

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

        If ``C2_RELAY_ANCHOR_ADDRESS`` is set, the CRM is also unregistered
        from the relay through the Rust relay control client.

        Parameters
        ----------
        name:
            The routing name used during :func:`register`.
        """
        with self._lock:
            server = self._server
            runtime_session = self._runtime_session
        if server is None:
            raise KeyError(f'Name not registered: {name!r}')

        outcome = server.unregister_crm(
            name,
            runtime_session=runtime_session,
            relay_anchor_address=None,
        )
        relay_error = outcome.get('relay_error')
        if relay_error is not None:
            raise _relay_cleanup_exception(dict(relay_error))

    def get_server_address(self) -> str | None:
        """IPC address of the auto-created server, or ``None``."""
        return self._runtime_session.server_address

    def get_server_id(self) -> str | None:
        """Server identity of the auto-created server, or ``None``."""
        return self._runtime_session.server_id

    @property
    def names(self) -> list[str]:
        """List of currently registered routing names."""
        with self._lock:
            server = self._server
        return server.names if server is not None else []

    def shutdown(self) -> None:
        """Full cleanup — shuts down server, terminates pooled clients.

        If ``C2_RELAY_ANCHOR_ADDRESS`` is set, all registered CRMs are
        unregistered from the relay before shutting down.

        Called automatically at process exit via :func:`atexit`.
        """
        with self._lock:
            server = self._server
            runtime_session = self._runtime_session
            from c_two._native import RuntimeSession
            self._runtime_session = RuntimeSession(**_runtime_session_kwargs_from_settings())
            self._server = None

        runtime_session.set_relay_anchor_address(settings._relay_anchor_address)  # noqa: SLF001

        if server is not None:
            try:
                outcome = server.shutdown(
                    runtime_session=runtime_session,
                    relay_anchor_address=None,
                )
            except Exception:
                log.warning('Error shutting down Server', exc_info=True)
                outcome = {'relay_errors': []}
        else:
            try:
                outcome = dict(runtime_session.shutdown(
                    None,
                    route_names=[],
                    relay_anchor_address=None,
                ))
            except Exception:
                log.warning('Error shutting down RuntimeSession', exc_info=True)
                outcome = {'relay_errors': []}

        for relay_error in outcome.get('relay_errors') or []:
            error = dict(relay_error)
            name = error.get('route_name')
            message = error.get('message') or 'unknown relay cleanup error'
            status = error.get('status_code')
            if isinstance(status, int):
                log.info(
                    'Relay cleanup failed during shutdown unregister of %s: '
                    'HTTP %s: %s',
                    name,
                    status,
                    message,
                )
            else:
                log.info(
                    'Relay unreachable during shutdown unregister of %s — %s',
                    name,
                    message,
                )

        self._runtime_session.shutdown_http_clients()
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
            server = self._server
            names = list(server.names) if server is not None else []
            addr = self._runtime_session.server_address

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
            while not self._serve_stop.wait(timeout=0.1):
                pass
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
    # Internals
    # ------------------------------------------------------------------

    def _server_started(self) -> bool:
        return self._server is not None and self._server.is_started()

    def _sync_relay_override(self) -> None:
        self._runtime_session.set_relay_anchor_address(settings._relay_anchor_address)  # noqa: SLF001



# ------------------------------------------------------------------
# Module-level API (delegates to singleton)
# ------------------------------------------------------------------

def set_transport_policy(
    *,
    shm_threshold: int | None | object = _UNSET,
    remote_payload_chunk_size: int | None | object = _UNSET,
) -> None:
    """Set process transport policy. Call before register()/connect()."""
    _ProcessRegistry.get().set_transport_policy(
        shm_threshold=shm_threshold,
        remote_payload_chunk_size=remote_payload_chunk_size,
    )


def set_server(
    *,
    server_id: str | None = None,
    ipc_overrides: ServerIPCOverrides | Mapping[str, object] | None = None,
) -> None:
    """Configure IPC server. Call before register()."""
    _ProcessRegistry.get().set_server(server_id=server_id, ipc_overrides=ipc_overrides)


def set_client(
    *,
    ipc_overrides: ClientIPCOverrides | Mapping[str, object] | None = None,
) -> None:
    """Configure IPC client. Call before connect()."""
    _ProcessRegistry.get().set_client(ipc_overrides=ipc_overrides)


def set_relay_anchor(address: str) -> None:
    """Set the relay anchor address for name resolution."""
    _ProcessRegistry.get().set_relay_anchor(address)


def register(
    crm_class: type,
    crm_instance: object,
    *,
    name: str,
    concurrency: ConcurrencyConfig | None = None,
    bridge: dict[str, ResourceBridge] | None = None,
    input_lifetime: Mapping[str, InputLifetimeLike] | None = None,
) -> str:
    """Register a CRM in the current process.

    See :meth:`_ProcessRegistry.register`.
    """
    return _ProcessRegistry.get().register(
        crm_class,
        crm_instance,
        name=name,
        concurrency=concurrency,
        bridge=bridge,
        input_lifetime=input_lifetime,
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


def server_id() -> str | None:
    """Server identity of the auto-created server, or ``None``."""
    return _ProcessRegistry.get().get_server_id()


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
    """Return retained runtime buffer tracking statistics.

    Returns dict with:
    - active_holds: number of currently retained runtime buffers
    - total_held_bytes: total retained runtime buffer bytes
    - oldest_hold_seconds: age of oldest active hold
    - by_storage: retained counts by inline, SHM, handle, and file-spill storage
    """
    inst = _ProcessRegistry.get()
    return dict(inst._runtime_session.hold_stats())  # noqa: SLF001


# Auto-cleanup on process exit.
atexit.register(lambda: _ProcessRegistry.reset())
