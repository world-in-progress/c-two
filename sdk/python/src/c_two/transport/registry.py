"""Runtime-scoped CRM registration and the default ``cc.register / cc.connect`` API.

Each :class:`Runtime` owns the existing native session and Python bindings:

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
to one default :class:`Runtime`; explicit instances use the same implementation.
"""
from __future__ import annotations

import atexit
import logging
import signal
import sys
import threading
from inspect import getattr_static
from collections.abc import Mapping
from contextlib import contextmanager
from typing import TYPE_CHECKING, TypeVar

from c_two.crm.contract import CRMContract, crm_contract, crm_contract_identity
from c_two.crm.conformance import validate_resource_conformance
from c_two.crm.meta import _SHUTDOWN_ATTR, get_shutdown_method
from c_two.crm.topic import topic_members
from c_two.mem import MemoryStats
from c_two.error import (
    CCError,
    ResourceNotFound,
    ResourceUnavailable,
    _project_native_error,
)
from .input_lifetime import _InputLifetime
from .client.proxy import CRMProxy, _FollowingLocalRoute
from ._snapshots import CallExecutionSnapshot, LifecycleSnapshot, ShutdownOutcome
from .server.scheduler import ConcurrencyConfig
from .server.native import NativeServerBridge as Server
from .topic import Registration, encode_topic_initial_value

if TYPE_CHECKING:
    from .endpoint import EndpointCredential

CRM = TypeVar('CRM')
log = logging.getLogger(__name__)
class _Unset:
    pass


_UNSET = _Unset()


def _print_serve_message(message: str) -> None:
    """Print and flush serve output using the current stdout's encoding."""
    stdout = sys.stdout
    if stdout is None:
        return
    encoding = getattr(stdout, 'encoding', None) or 'utf-8'
    message = message.encode(encoding, errors='backslashreplace').decode(encoding)
    print(message, file=stdout, flush=True)


def _relay_control_error_status(exc: BaseException) -> int | None:
    value = getattr(exc, "status_code", None)
    return value if isinstance(value, int) else None


def _is_missing_relay_address(exc: BaseException) -> bool:
    return getattr(exc, 'lifecycle_kind', None) == 'missing_relay_address'


def _cc_error_from_native_exception(exc: BaseException) -> CCError | None:
    """Project the canonical semantic error of one native failure."""
    return _project_native_error(exc)


def _connect_native_call(operation, *args, **kwargs):
    """Project Core's canonical connection error without creating timeout policy."""
    try:
        return operation(*args, **kwargs)
    except Exception as exc:
        if (cc_err := _cc_error_from_native_exception(exc)) is not None:
            try:
                raise cc_err from exc
            finally:
                del cc_err
        raise


@contextmanager
def _connect_lock(lock, connect_attempt, stage: str):
    """The native attempt owns timing; Python only releases its acquired lock."""
    if connect_attempt is None:
        lock.acquire()
    else:
        _connect_native_call(connect_attempt.acquire_lock, lock, stage)
    try:
        yield
    finally:
        lock.release()


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


class Runtime:
    """Independent CRM bindings over one native runtime session.

    Configuration, routes and observations belong to this instance. A completed
    ``shutdown`` preserves the existing session-replacement behavior, so this
    object can be configured and used again. Endpoint directories remain owned
    by their caller; Runtime never removes them.
    """

    def __init__(self) -> None:
        from c_two._native import RuntimeSession

        self._lock = threading.Lock()
        self._server: Server | None = None
        self._runtime_session = RuntimeSession()

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def set_local_endpoint(self, *, root: str | None = None) -> None:
        """Forward the code root to Core's endpoint resolver."""
        if root is not None and not isinstance(root, str):
            raise TypeError('root must be a str or None')
        with self._lock:
            self._runtime_session.set_local_endpoint(root=root)

    def set_relay_anchor(self, address: str | None) -> None:
        """Set the relay anchor override or clear it with None."""
        if isinstance(address, str):
            address = address.strip()
            if not address:
                raise ValueError('relay anchor address must not be empty; pass None to clear')
        elif address is not None:
            raise TypeError('relay anchor address must be a str or None')
        with self._lock:
            self._runtime_session.set_relay_anchor_address(address)

    def _swap_runtime_session(self) -> None:
        """Publish a replacement after completed retirement, under the binding lock."""
        previous = self._runtime_session
        replacement = type(previous)()
        replacement.inherit_local_endpoint_selection(previous)
        replacement.inherit_retirement(previous)
        replacement.set_relay_anchor_address(previous.relay_anchor_address_override)
        self._runtime_session = replacement
        self._server = None

    def set_server(
        self,
        *,
        server_id: str | None | _Unset = _UNSET,
        startup_timeout: float | None | _Unset = _UNSET,
        max_payload_size: int | None | _Unset = _UNSET,
        max_execution_workers: int | None | _Unset = _UNSET,
        memory_budget_bytes: int | None | _Unset = _UNSET,
        file_spill_budget_bytes: int | None | _Unset = _UNSET,
        max_outstanding_calls: int | None | _Unset = _UNSET,
        outcome_max_entries: int | None | _Unset = _UNSET,
        outcome_max_retained_bytes: int | None | _Unset = _UNSET,
    ) -> None:
        """Update Host configuration. Omitted fields keep their overrides; None inherits."""
        self._runtime_session.update_server_config(
            _UNSET, server_id, startup_timeout, max_payload_size, max_execution_workers,
            memory_budget_bytes, file_spill_budget_bytes, max_outstanding_calls,
            outcome_max_entries, outcome_max_retained_bytes,
        )

    def set_client(
        self,
        *,
        memory_budget_bytes: int | None | _Unset = _UNSET,
        file_spill_budget_bytes: int | None | _Unset = _UNSET,
        max_outstanding_calls: int | None | _Unset = _UNSET,
    ) -> None:
        """Update client capacity. Omitted fields keep their overrides; None inherits."""
        self._runtime_session.update_client_config(
            _UNSET, memory_budget_bytes, file_spill_budget_bytes, max_outstanding_calls,
        )

    def _ensure_server_bridge(self) -> Server:
        # Called under the existing instance lock, by register and empty start.
        if self._server is None:
            try:
                self._server = self._runtime_session.ensure_server_bridge()
            except Exception:
                self._runtime_session.clear_server_identity()
                raise
        return self._server

    @contextmanager
    def _active_binding(self):
        with self._lock:
            session = self._runtime_session
            if not session.retired:
                yield
                return
        # Core retirement is irreversible. Project its original refusal only
        # after unlocking, since an exception initializer may read this Runtime.
        session.check_active()

    def start(self) -> None:
        """Start this instance's Host and await its native readiness/PONG."""
        with self._active_binding():
            self._ensure_server_bridge().start()

    def local_endpoint_context(self):
        """Read this instance's native context without creating or freezing it."""
        with self._lock:
            return self._runtime_session.local_endpoint_context()

    def connection_events(self):
        """Subscribe to this session's native physical connection events."""
        with self._lock:
            return self._runtime_session.connection_events()

    def memory_stats(self) -> MemoryStats:
        """Observe role-domain memory and local holds."""
        return self._runtime_session.memory_stats()

    def call_execution_snapshot(self) -> CallExecutionSnapshot:
        """Observe role-domain operation slots."""
        return self._runtime_session.call_execution_snapshot()

    def register(
        self,
        crm_class: type,
        crm_instance: object,
        *,
        name: str,
        concurrency: ConcurrencyConfig | None = None,
        input_lifetime: Mapping[str, _InputLifetime] | None = None,
        topic_capacity: Mapping[str, int] | None = None,
        initial_topics: Mapping[str, object] | None = None,
    ) -> Registration:
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
        topic_capacity:
            Optional bounded history ring depth (``>= 1``) per declared topic.
        initial_topics:
            Optional initial value per declared topic, committed atomically
            with the same publish logic before the route opens; a failure
            rolls the registration back.

        Returns
        -------
        Registration
            The registration handle. The Runtime keeps the route alive;
            dropping the returned object never unregisters it.
        """
        crm_contract_identity(crm_class)
        validate_resource_conformance(
            crm_class,
            crm_instance,
        )
        topics = topic_members(crm_class)
        if topic_capacity is not None:
            unknown = sorted(set(topic_capacity) - set(topics))
            if unknown:
                raise ValueError(
                    f'topic_capacity names topics not declared on '
                    f'{crm_class.__name__}: {", ".join(unknown)}',
                )
        serialized_initial: dict[str, bytes] = {}
        if initial_topics is not None:
            unknown = sorted(set(initial_topics) - set(topics))
            if unknown:
                raise ValueError(
                    f'initial_topics names topics not declared on '
                    f'{crm_class.__name__}: {", ".join(unknown)}',
                )
            # Serialize the initial frames before touching any server state,
            # so a codec failure leaves the route completely unopened.
            for topic_name, value in initial_topics.items():
                serialized_initial[topic_name] = encode_topic_initial_value(
                    topics[topic_name].binding, value,
                )
        shutdown_method = get_shutdown_method(crm_class)
        resource_type = type(crm_instance)
        resource_members = {
            name for base in resource_type.__mro__ for name in vars(base)
        }
        for method_name in sorted(resource_members):
            method = getattr_static(resource_type, method_name)
            marked = getattr_static(method, _SHUTDOWN_ATTR, False) is True
            if isinstance(method, (staticmethod, classmethod)):
                marked = marked or (
                    getattr_static(method.__func__, _SHUTDOWN_ATTR, False) is True
                )
            if marked and method_name != shutdown_method:
                raise TypeError(
                    f'@cc.on_shutdown on resource '
                    f'{type(crm_instance).__name__}.{method_name} must be '
                    f'declared with @cc.on_shutdown on CRM {crm_class.__name__}.',
                )
        with self._active_binding():
            created_server = False
            try:
                # Lazy-init server on first registration. Native identity is
                # cleared if Python server bridge construction fails, so invalid
                # server config does not leave a visible server id/address.
                server = self._server
                if server is None:
                    server = self._ensure_server_bridge()
                    created_server = True

                native_registration = server.register_crm(
                    crm_class,
                    crm_instance,
                    concurrency,
                    name=name,
                    input_lifetime=input_lifetime,
                    topic_capacity=dict(topic_capacity) if topic_capacity is not None else None,
                    initial_topics=serialized_initial,
                )

                # Start server if not yet running.
                if not server.is_started():
                    server.start()

                server_address = self._runtime_session.server_address
            except Exception:
                if created_server:
                    server = self._server
                    self._server = None
                    if server is not None:
                        try:
                            server.shutdown()
                        except Exception:
                            log.warning('Error shutting down Server after failed register', exc_info=True)
                    try:
                        self._runtime_session.clear_server_identity()
                    except Exception:
                        log.warning(
                            'Error clearing Core identity after failed register',
                            exc_info=True,
                        )
                raise

        log.debug('Registered CRM %s at %s', name, server_address)

        return Registration(
            native_registration,
            {topic_name: info.binding for topic_name, info in topics.items()},
        )

    def connect(
        self,
        crm_class: type[CRM],
        *,
        name: str,
        address: str | None = None,
        timeout: float | None = None,
    ) -> CRM:
        """Obtain a CRM instance connected to a registered resource.

        **Thread preference**: when the target *name* is registered in
        this Runtime and no explicit *address* is given, returns a
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
        timeout:
            Total connection-operation budget in seconds. ``None``
            preserves the existing phase guards. A finite non-negative value
            bounds this connection operation; zero expires immediately.
            This does not change timeouts on subsequent business calls.

        Returns
        -------
        object
            A CRM instance with ``.client`` set to an
            :class:`CRMProxy`.
        """
        from c_two._native import ConnectAttempt

        attempt = _connect_native_call(ConnectAttempt, timeout_seconds=timeout)
        return self._connect(crm_class, name=name, address=address, attempt=attempt)

    def _connect(self, crm_class: type[CRM], *, name: str, address: str | None, attempt) -> CRM:
        # Module-level connect creates this same attempt before the default lock.
        _connect_native_call(attempt.check, 'python_contract')
        expected_contract = crm_contract(crm_class)
        topic_bindings = {
            topic_name: info.binding
            for topic_name, info in topic_members(crm_class).items()
        }
        with _connect_lock(self._lock, attempt, 'registry_snapshot_wait'):
            server = self._server
            runtime_session = self._runtime_session
            local = (
                _connect_native_call(server.get_local_slot_info, name, connect_attempt=attempt)
                if address is None and server is not None
                else None
            )
            if local is not None and local[1] is not None and local[1].snapshot().closed:
                local = None
            local_copy_tracker = runtime_session.local_copy_tracker()

            remote = address is not None or local is None

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
                name=name,
                scheduler=scheduler,
                local_copy_tracker=local_copy_tracker,
                topic_bindings=topic_bindings,
                following_local=_FollowingLocalRoute(
                    self, runtime_session, crm_class, expected_contract, name, crm_instance, scheduler,
                ),
            )
        elif address is not None and address.startswith(('http://', 'https://')):
            # HTTP mode — cross-node via relay server.
            try:
                client = runtime_session.connect_explicit_relay_http(
                    address,
                    name,
                    *expected_contract.native_args(),
                    connect_attempt=attempt,
                )
            except Exception as exc:
                if (cc_err := _cc_error_from_native_exception(exc)) is not None:
                    try:
                        raise cc_err from exc
                    finally:
                        del cc_err
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
                local_copy_tracker=local_copy_tracker,
                topic_bindings=topic_bindings,
            )
        elif address is not None:
            # Remote IPC via pooled RustClient.
            try:
                client = runtime_session.acquire_ipc_client(
                    address,
                    name,
                    *expected_contract.native_args(),
                    connect_attempt=attempt,
                )
            except Exception as exc:
                if (cc_err := _cc_error_from_native_exception(exc)) is not None:
                    try:
                        raise cc_err from exc
                    finally:
                        del cc_err
                raise
            proxy = CRMProxy.ipc(
                client,
                name,
                on_terminate=client.close,
                local_copy_tracker=local_copy_tracker,
                topic_bindings=topic_bindings,
            )
        else:
            try:
                client = runtime_session.connect_via_relay(
                    name,
                    *expected_contract.native_args(),
                    connect_attempt=attempt,
                )
            except Exception as exc:
                if (cc_err := _cc_error_from_native_exception(exc)) is not None:
                    try:
                        raise cc_err from exc
                    finally:
                        del cc_err
                if _is_missing_relay_address(exc):
                    raise LookupError(
                        f'Name {name!r} is not registered locally '
                        f'and no address was provided',
                    ) from exc
                status = _relay_control_error_status(exc)
                if status == 404:
                    raise ResourceNotFound(f"Resource '{name}' not found") from exc
                if status is not None:
                    raise ResourceUnavailable(f"Relay error: {status}") from exc
                raise
            mode = getattr(client, 'mode', 'http')
            if mode == 'ipc':
                proxy = CRMProxy.ipc(
                    client,
                    name,
                    on_terminate=lambda: client.close(),
                    local_copy_tracker=local_copy_tracker,
                    topic_bindings=topic_bindings,
                )
            elif mode == 'http':
                proxy = CRMProxy.http(
                    client,
                    name,
                    on_terminate=lambda: client.close(),
                    local_copy_tracker=local_copy_tracker,
                    topic_bindings=topic_bindings,
                )
            else:
                raise RuntimeError(f"Unsupported relay target mode: {mode!r}")

        try:
            crm = crm_class()
            crm.client = proxy
            _connect_native_call(attempt.check, 'python_connect_finish')
            if remote:
                with _connect_lock(self._lock, attempt, 'python_connect_finish'):
                    same_session = self._runtime_session is runtime_session
                _connect_native_call(runtime_session.check_active, connect_attempt=attempt)
                if not same_session:
                    raise RuntimeError('Runtime session was replaced during connect')
            return crm
        except Exception:
            proxy.terminate()
            raise

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
        if server is None:
            raise KeyError(f'Name not registered: {name!r}')

        outcome = server.unregister_crm(name)
        relay_error = outcome.get('relay_error')
        if relay_error is not None:
            raise _relay_cleanup_exception(dict(relay_error))

    def server_address(self) -> str | None:
        """IPC address of the auto-created server, or ``None``."""
        return self._runtime_session.server_address

    def server_id(self) -> str | None:
        """Server identity of the auto-created server, or ``None``."""
        return self._runtime_session.server_id

    def server_credential(self) -> EndpointCredential | None:
        """Credential captured from the bound listener, if one is present."""
        return self._runtime_session.server_credential

    @property
    def names(self) -> list[str]:
        """List of currently registered routing names."""
        with self._lock:
            server = self._server
        return server.names if server is not None else []

    def shutdown(self, *, timeout: float = 30.0) -> ShutdownOutcome:
        """Consume native shutdown; retain the session until Core proves completion."""
        with self._lock:
            server = self._server
            runtime_session = self._runtime_session
            observation = runtime_session.begin_shutdown(timeout_seconds=timeout)
        outcome = dict(observation.observe())
        for key, description in (
                ('ipc_client_close_error', 'IPC client cache'),
                ('runtime_barrier_error', 'runtime barrier')):
            if outcome.get(key):
                log.warning('Shutdown could not confirm %s cleanup: %s', description, outcome[key])
        if not outcome['completed']:
            return outcome
        slots = []
        with self._lock:
            if self._runtime_session is runtime_session:
                self._swap_runtime_session()
                if server is not None:
                    slots = server._take_shutdown_slots(outcome)
                if self._serve_stop is not None:
                    self._serve_stop.set()
                    self._serve_stop = None

        for slot in slots:
            server._invoke_shutdown(slot)

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

        return outcome

    # ------------------------------------------------------------------
    # Serve (daemon mode)
    # ------------------------------------------------------------------

    _serve_stop: threading.Event | None = None

    def _serve(
        self, blocking: bool = True, *, shutdown_timeout: float = 30.0,
    ) -> ShutdownOutcome | None:
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
        shutdown_timeout:
            Maximum seconds this caller waits for shutdown after a signal.
            The native cleanup continues when the budget expires.
        """
        self._runtime_session.validate_shutdown_timeout(shutdown_timeout)
        if self._serve_stop is not None:
            return  # idempotent

        self._serve_stop = threading.Event()
        serve_stop = self._serve_stop

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
            serve_stop.set()

        try:
            signal.signal(signal.SIGINT, _handle_signal)
            if sys.platform == 'win32':
                signal.signal(signal.SIGBREAK, _handle_signal)
            else:
                signal.signal(signal.SIGTERM, _handle_signal)
        except ValueError:
            # Not the main thread — signals cannot be registered.
            # Caller must arrange to call _serve_stop.set() externally.
            log.debug('cc.serve(): signal handlers skipped (not main thread)')

        # Print banner (after signal handlers are ready).
        self._print_serve_banner(names, addr)

        if not blocking:
            return

        # The native lifecycle is the truth: the Host can stop itself,
        # for example after a direct IPC shutdown request, without any signal,
        # so serve() must also watch the native terminal outcome instead of
        # polling only for a stop request.
        while not serve_stop.wait(timeout=0.1):
            if self._native_host_terminated():
                break

        # One caller budget bounds shutdown observation. A pending result
        # leaves the native retirement driver running for a later shutdown
        # call to consume.
        _print_serve_message('\n Shutting down...')
        try:
            outcome = self.shutdown(timeout=shutdown_timeout)
        finally:
            self._serve_stop = None
        if outcome['completed']:
            _print_serve_message(' Resource server stopped.')
        else:
            _print_serve_message(' Resource server shutdown is still in progress.')
        return outcome

    def _native_host_terminated(self) -> bool:
        """Whether the Core host reached its structured terminal outcome."""
        with self._lock:
            server = self._server
        if server is None:
            return False
        return bool(server.shutdown_outcome() is not None)

    @staticmethod
    def _print_serve_banner(
        names: list[str],
        addr: str | None,
    ) -> None:
        """Print a human-readable startup banner to stdout."""
        lines = [' -- C-Two Resource Server ----------------']
        if addr:
            lines.append(f'  IPC: {addr}')
        if names:
            lines.append('  CRM routes:')
            for n in names:
                lines.append(f'    - {n}')
        else:
            lines.append('  (no CRM routes registered)')
        lines.append(' -----------------------------------------')
        count = len(names)
        lines.append(f'  Serving {count} CRM resource(s). Ctrl+C to stop.')
        _print_serve_message('\n'.join(lines))

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _server_started(self) -> bool:
        return self._server is not None and self._server.is_started()

    def lifecycle_snapshot(self) -> LifecycleSnapshot | None:
        """Read the current native Host lifecycle, if any."""
        return self._runtime_session.native_lifecycle_snapshot()

    def shutdown_outcome(self) -> ShutdownOutcome | None:
        """Read the current native Host terminal outcome, if any."""
        return self._runtime_session.native_terminal_outcome()


# ------------------------------------------------------------------
# Module-level API (delegates to the default Runtime)
# ------------------------------------------------------------------

_default_runtime: Runtime | None = None
_default_runtime_lock = threading.Lock()


def _get_default_runtime(*, connect_attempt=None) -> Runtime:
    global _default_runtime
    if _default_runtime is None:
        with _connect_lock(_default_runtime_lock, connect_attempt, 'registry_init_wait'):
            if _default_runtime is None:
                _default_runtime = Runtime()
    if connect_attempt is not None:
        _connect_native_call(connect_attempt.check, 'registry_init')
    return _default_runtime


def _reset_default_runtime() -> None:
    global _default_runtime
    with _default_runtime_lock:
        instance = _default_runtime
    if instance is not None and instance.shutdown()['completed']:
        with _default_runtime_lock:
            if _default_runtime is instance:
                _default_runtime = None


def set_local_endpoint(*, root: str | None = None) -> None:
    """Configure local IPC through Core. Call before the first local attempt.

    Code root overrides environment/.env/defaults. Queries do not create
    directories or freeze the selection; the first bind/connect attempt does,
    including a failed attempt. Repeating the same frozen context is a no-op.
    The Unix root container must already exist; Windows rejects root overrides.
    Completed shutdown preserves the native selection in the fresh session.
    """
    _get_default_runtime().set_local_endpoint(root=root)


def set_server(
    *,
    server_id: str | None | _Unset = _UNSET,
    startup_timeout: float | None | _Unset = _UNSET,
    max_payload_size: int | None | _Unset = _UNSET,
    max_execution_workers: int | None | _Unset = _UNSET,
    memory_budget_bytes: int | None | _Unset = _UNSET,
    file_spill_budget_bytes: int | None | _Unset = _UNSET,
    max_outstanding_calls: int | None | _Unset = _UNSET,
    outcome_max_entries: int | None | _Unset = _UNSET,
    outcome_max_retained_bytes: int | None | _Unset = _UNSET,
) -> None:
    """Update the default Runtime's Host configuration."""
    _get_default_runtime().set_server(
        server_id=server_id,
        startup_timeout=startup_timeout,
        max_payload_size=max_payload_size,
        max_execution_workers=max_execution_workers,
        memory_budget_bytes=memory_budget_bytes,
        file_spill_budget_bytes=file_spill_budget_bytes,
        max_outstanding_calls=max_outstanding_calls,
        outcome_max_entries=outcome_max_entries,
        outcome_max_retained_bytes=outcome_max_retained_bytes,
    )


def set_client(
    *,
    memory_budget_bytes: int | None | _Unset = _UNSET,
    file_spill_budget_bytes: int | None | _Unset = _UNSET,
    max_outstanding_calls: int | None | _Unset = _UNSET,
) -> None:
    """Update the default Runtime's client capacity."""
    _get_default_runtime().set_client(
        memory_budget_bytes=memory_budget_bytes,
        file_spill_budget_bytes=file_spill_budget_bytes,
        max_outstanding_calls=max_outstanding_calls,
    )


def set_relay_anchor(address: str | None) -> None:
    """Set the default Runtime relay anchor override or clear it with None."""
    _get_default_runtime().set_relay_anchor(address)


def register(
    crm_class: type,
    crm_instance: object,
    *,
    name: str,
    concurrency: ConcurrencyConfig | None = None,
    input_lifetime: Mapping[str, _InputLifetime] | None = None,
    topic_capacity: Mapping[str, int] | None = None,
    initial_topics: Mapping[str, object] | None = None,
) -> Registration:
    """Register a CRM in the current process.

    See :meth:`Runtime.register`.
    """
    return _get_default_runtime().register(
        crm_class,
        crm_instance,
        name=name,
        concurrency=concurrency,
        input_lifetime=input_lifetime,
        topic_capacity=topic_capacity,
        initial_topics=initial_topics,
    )


def connect(
    crm_class: type[CRM],
    *,
    name: str,
    address: str | None = None,
    timeout: float | None = None,
) -> CRM:
    """Obtain a CRM proxy for a registered resource.

    See :meth:`Runtime.connect`.
    """
    from c_two._native import ConnectAttempt

    attempt = _connect_native_call(ConnectAttempt, timeout_seconds=timeout)
    registry = _get_default_runtime(connect_attempt=attempt)
    return registry._connect(
        crm_class, name=name, address=address, attempt=attempt,
    )


def close(crm: object) -> None:
    """Close a connection obtained from :func:`connect`.

    The proxy carries its own native client and release callback.
    """
    proxy = getattr(crm, 'client', None)
    if not isinstance(proxy, CRMProxy):
        raise TypeError('cc.close() requires a connected CRM instance')
    proxy.terminate()


def unregister(name: str) -> None:
    """Remove a CRM from the registry.

    See :meth:`Runtime.unregister`.
    """
    _get_default_runtime().unregister(name)


def server_address() -> str | None:
    """IPC address of the auto-created server, or ``None``."""
    return _get_default_runtime().server_address()


def server_id() -> str | None:
    """Server identity of the auto-created server, or ``None``."""
    return _get_default_runtime().server_id()


def server_credential() -> EndpointCredential | None:
    """Return the bound listener's credential snapshot, if one is present."""
    return _get_default_runtime().server_credential()


def shutdown(*, timeout: float = 30.0) -> ShutdownOutcome:
    """Full cleanup — shuts down server and pooled clients.

    See :meth:`Runtime.shutdown`.
    """
    return _get_default_runtime().shutdown(timeout=timeout)


def connection_events():
    """Subscribe to outgoing and accepted IPC connections without starting or configuring a server."""
    return _get_default_runtime().connection_events()


def lifecycle_snapshot() -> LifecycleSnapshot | None:
    """Thin read-only Core host lifecycle observation, or ``None``."""
    return _get_default_runtime().lifecycle_snapshot()


def shutdown_outcome() -> ShutdownOutcome | None:
    """Terminal native shutdown outcome, or ``None`` while not terminal."""
    return _get_default_runtime().shutdown_outcome()


def serve(
    blocking: bool = True, *, shutdown_timeout: float = 30.0,
) -> ShutdownOutcome | None:
    """Keep the process alive as a CRM resource server.

    Transitions the calling process from a computation script into a
    long-running resource service.  Blocks until ``SIGINT`` or
    ``SIGTERM``, then calls :func:`shutdown` with ``shutdown_timeout``.
    Returns the shutdown outcome, including ``completed=False`` if the caller's
    wait budget expires while native cleanup continues. Nonblocking calls
    return ``None``.

    Explicit Runtime instances use ``start`` and let their application own signals.
    """
    return _get_default_runtime()._serve(
        blocking=blocking, shutdown_timeout=shutdown_timeout,
    )


def call_execution_snapshot() -> CallExecutionSnapshot:
    """Observe client, server and retired operation slots without activating either role."""
    return _get_default_runtime().call_execution_snapshot()


def memory_stats() -> MemoryStats:
    """Observe client, server and retired memory budgets and local holds."""
    return _get_default_runtime().memory_stats()


# Auto-cleanup on process exit.
atexit.register(_reset_default_runtime)
