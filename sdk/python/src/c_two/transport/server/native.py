"""Native server bridge — Python wrapper around ``RustServer``.

Provides the Python CRM dispatch surface while delegating transport to the
Rust ``c2-server`` crate via PyO3 bindings.

CRM domain logic (CRM instance creation, method discovery, dispatch tables,
shutdown callbacks) remains in Python.  Only the UDS accept loop,
frame parsing, heartbeat, and concurrency scheduling move to Rust.
"""
from __future__ import annotations

import logging
import math
import threading
from collections.abc import Mapping
from dataclasses import dataclass, field
from typing import Any, Callable

from ...crm.bridge import ResourceBridge, normalize_bridge_map, wrap_resource
from ...crm.contract import (
    CRMContract,
    crm_contract_identity,
    crm_route_contract_hashes,
)
from ...crm.methods import rpc_method_names
from ...crm.meta import MethodAccess, get_method_access, get_shutdown_method
from ...crm.payload_plan import PayloadPlanKind
from ...error import ResourceAlreadyRegistered
from c_two.config.ipc import ServerIPCOverrides, _resolve_server_ipc_config
from ..input_lifetime import (
    InputLifetime,
    InputLifetimeLike,
    normalize_input_lifetime_map,
    validate_input_lifetime_resource_contract,
)
from ..wire import MethodTable
from .scheduler import ConcurrencyConfig, Scheduler
from .reply import unpack_resource_result

logger = logging.getLogger(__name__)


class CrmCallError(Exception):
    """Raised by the dispatch callable when a CRM method returns an error.

    The ``error_bytes`` attribute carries the serialized CCError so the
    Rust ``PyCrmCallback`` can forward them as ``CrmError::UserError``
    without losing the binary format.
    """

    def __init__(self, error_bytes: bytes) -> None:
        super().__init__(f'CRM error ({len(error_bytes)} bytes)')
        self.error_bytes = error_bytes


@dataclass
class CRMSlot:
    """Per-CRM registration, keyed by routing name."""

    name: str
    crm_instance: object
    direct_instance: object
    crm_ns: str
    crm_name: str
    crm_ver: str
    abi_hash: str
    signature_hash: str
    method_table: MethodTable
    scheduler: Scheduler
    methods: list[str]
    shutdown_method: str | None = None
    input_lifetime: dict[str, InputLifetime] = field(default_factory=dict)
    _dispatch_table: dict[str, tuple[Any, MethodAccess, str]] = field(
        default_factory=dict, repr=False,
    )

    def build_dispatch_table(self) -> None:
        for name in self.methods:
            if name == self.shutdown_method:
                continue
            method = getattr(self.crm_instance, name, None)
            if method is not None:
                access = get_method_access(method)
                lifetime = self.input_lifetime.get(name)
                if lifetime is InputLifetime.BORROWED:
                    binding = getattr(method, '_input_payload_binding', None)
                    if (
                        getattr(binding, 'kind', None) is not PayloadPlanKind.FDB
                        or getattr(binding, 'view_from_buffer', None) is None
                    ):
                        raise ValueError(
                            f'input_lifetime BORROWED for {name!r} requires a buffer-view FDB input payload',
                        )
                    buffer_mode = 'borrowed'
                elif lifetime is InputLifetime.MATERIALIZED:
                    buffer_mode = 'view'
                else:
                    buffer_mode = getattr(method, '_input_buffer_mode', 'view')
                    if buffer_mode == 'hold':
                        raise ValueError(
                            "server-side borrowed input is controlled by "
                            "cc.register(..., input_lifetime=...), not @cc.transfer(buffer='hold')",
                        )
                self._dispatch_table[name] = (method, access, buffer_mode)


def _session_has_relay(runtime_session: object, relay_anchor_address: str | None) -> bool:
    if relay_anchor_address is not None:
        return True
    value = getattr(runtime_session, 'effective_relay_anchor_address', None)
    if value is None:
        return False
    try:
        return value() is not None if callable(value) else value is not None
    except Exception:
        return False


def _new_standalone_runtime_session() -> object:
    from c_two._native import RuntimeSession
    return RuntimeSession(use_process_relay_anchor=False)


def _ensure_no_standalone_relay(
    runtime_session: object | None,
    relay_anchor_address: str | None,
) -> None:
    if runtime_session is None and relay_anchor_address is not None:
        raise ValueError(
            'relay operations require a runtime_session-owned server bridge',
        )


def _close_outcome_by_route(outcome: Mapping[str, Any]) -> dict[str, Mapping[str, Any]]:
    route_outcomes = outcome.get('route_outcomes') or ()
    by_route: dict[str, Mapping[str, Any]] = {}
    for close in route_outcomes:
        if not isinstance(close, Mapping):
            continue
        route_name = close.get('route_name')
        if isinstance(route_name, str) and route_name:
            by_route[route_name] = close
    return by_route


def _close_outcome_is_hook_safe(close: Mapping[str, Any] | None) -> bool:
    if not isinstance(close, Mapping):
        return False
    if not bool(close.get('local_removed', False)):
        return False
    if not bool(close.get('active_drained', False)):
        return False
    return close.get('closed_reason') != 'registration_rollback'

class NativeServerBridge:
    """Drop-in replacement for the Python ``Server`` class.

    Manages CRM slots in Python (CRM instance creation, dispatch tables,
    shutdown callbacks) while delegating the IPC transport to the
    Rust ``RustServer`` (c2-server crate).
    """

    def __init__(
        self,
        bind_address: str,
        *,
        ipc_overrides: ServerIPCOverrides | Mapping[str, object] | None = None,
        server_id: str | None = None,
        server_instance_id: str | None = None,
        hold_warn_seconds: float = 60.0,
        lease_tracker: object | None = None,
    ) -> None:
        self._config = _resolve_server_ipc_config(ipc_overrides)
        self._address = bind_address

        self._slots: dict[str, CRMSlot] = {}
        self._slots_lock = threading.Lock()

        if lease_tracker is None:
            from c_two._native import BufferLeaseTracker
            lease_tracker = BufferLeaseTracker()
        self._lease_tracker = lease_tracker
        self._hold_warn_seconds = float(hold_warn_seconds)
        if (
            not math.isfinite(self._hold_warn_seconds)
            or self._hold_warn_seconds < 0.0
        ):
            raise ValueError(
                'hold_warn_seconds must be a non-negative finite number',
            )
        self._hold_sweep_interval = 10

        from c_two._native import RustServer

        self._rust_server = RustServer(
            address=bind_address,
            shm_threshold=int(self._config['shm_threshold']),
            pool_enabled=self._config['pool_enabled'],
            pool_segment_size=self._config['pool_segment_size'],
            max_pool_segments=self._config['max_pool_segments'],
            reassembly_segment_size=self._config['reassembly_segment_size'],
            reassembly_max_segments=self._config['reassembly_max_segments'],
            max_frame_size=self._config['max_frame_size'],
            max_payload_size=self._config['max_payload_size'],
            max_pending_requests=self._config['max_pending_requests'],
            max_execution_workers=self._config['max_execution_workers'],
            pool_decay_seconds=self._config['pool_decay_seconds'],
            heartbeat_interval=self._config['heartbeat_interval'],
            heartbeat_timeout=self._config['heartbeat_timeout'],
            max_total_chunks=self._config['max_total_chunks'],
            chunk_gc_interval=self._config['chunk_gc_interval'],
            chunk_threshold_ratio=self._config['chunk_threshold_ratio'],
            chunk_assembler_timeout=self._config['chunk_assembler_timeout'],
            max_reassembly_bytes=self._config['max_reassembly_bytes'],
            chunk_size=self._config['chunk_size'],
            server_id=server_id,
            server_instance_id=server_instance_id,
        )

    # ------------------------------------------------------------------
    # CRM registration
    # ------------------------------------------------------------------

    def register_crm(
        self,
        crm_class: type,
        crm_instance: object,
        concurrency: ConcurrencyConfig | None = None,
        *,
        name: str,
        bridge: dict[str, ResourceBridge] | None = None,
        input_lifetime: Mapping[str, InputLifetimeLike] | None = None,
        runtime_session: object | None = None,
        relay_anchor_address: str | None = None,
    ) -> str:
        crm_ns, crm_name, crm_ver = self._extract_contract_identity(crm_class)
        if not isinstance(name, str):
            raise TypeError('name must be an explicit route name string')
        routing_name = name
        methods = self._discover_methods(crm_class)
        method_bridge_map = normalize_bridge_map(bridge, method_names=methods)
        input_lifetime_map = normalize_input_lifetime_map(
            input_lifetime,
            method_names=methods,
        )
        validate_input_lifetime_resource_contract(
            crm_class,
            crm_instance,
            input_lifetime_map,
            bridge=method_bridge_map,
        )
        runtime_resource = wrap_resource(crm_instance, method_bridge_map)
        instance = self._create_crm_instance(crm_class, runtime_resource)
        cc_config = concurrency or ConcurrencyConfig()
        sd_method = get_shutdown_method(crm_class)

        if sd_method is not None:
            methods = [m for m in methods if m != sd_method]
        method_table = MethodTable.from_methods(methods)
        abi_hash, signature_hash = self._route_contract_hashes(
            crm_ns,
            crm_name,
            crm_ver,
            methods,
            crm_class,
        )

        slot = CRMSlot(
            name=routing_name,
            crm_instance=instance,
            direct_instance=runtime_resource,
            crm_ns=crm_ns,
            crm_name=crm_name,
            crm_ver=crm_ver,
            abi_hash=abi_hash,
            signature_hash=signature_hash,
            method_table=method_table,
            scheduler=None,  # type: ignore[arg-type]
            methods=methods,
            shutdown_method=sd_method,
            input_lifetime=input_lifetime_map,
        )
        slot.build_dispatch_table()

        # Build access map: method_idx → "read"|"write"
        access_map: dict[int, str] = {}
        for idx, mname in enumerate(methods):
            entry = slot._dispatch_table.get(mname)
            if entry is not None:
                _, access, _bm = entry
                access_map[idx] = (
                    'read' if access is MethodAccess.READ else 'write'
                )

        dispatcher = self._make_dispatcher(routing_name, slot)
        method_index = {mname: idx for idx, mname in enumerate(methods)}

        with self._slots_lock:
            if routing_name in self._slots:
                raise ValueError(f'Name already registered: {routing_name!r}')
            _ensure_no_standalone_relay(runtime_session, relay_anchor_address)
            if runtime_session is None:
                runtime_session = _new_standalone_runtime_session()
            if _session_has_relay(runtime_session, relay_anchor_address) and not self.is_started():
                self.start()
            try:
                _outcome, native_concurrency = runtime_session.register_route(
                    self._rust_server,
                    routing_name,
                    dispatcher,
                    methods,
                    access_map,
                    cc_config.mode.value,
                    cc_config.max_pending,
                    cc_config.max_workers,
                    crm_ns,
                    crm_name,
                    crm_ver,
                    abi_hash,
                    signature_hash,
                    relay_anchor_address,
                )
            except Exception as exc:
                if (
                    getattr(exc, 'status_code', None) == 409
                    and getattr(exc, 'relay_duplicate', False)
                ):
                    raise ResourceAlreadyRegistered(
                        f'Route name already registered with relay: {routing_name!r}',
                    ) from exc
                if getattr(exc, 'status_code', None) == 409:
                    raise ValueError(str(exc)) from exc
                raise
            slot.scheduler = Scheduler(native_concurrency, method_index)
            self._slots[routing_name] = slot
        return routing_name

    def unregister_crm(
        self,
        name: str,
        *,
        runtime_session: object | None = None,
        relay_anchor_address: str | None = None,
    ) -> dict[str, Any]:
        with self._slots_lock:
            slot = self._slots.get(name)
            if slot is None:
                raise KeyError(f'Name not registered: {name!r}')

        _ensure_no_standalone_relay(runtime_session, relay_anchor_address)
        if runtime_session is None:
            runtime_session = _new_standalone_runtime_session()

        outcome = dict(runtime_session.unregister_route(
            self._rust_server,
            name,
            relay_anchor_address,
        ))
        if not outcome.get('local_removed', False):
            raise KeyError(f'Name not registered in native server: {name!r}')
        close = outcome.get('close')
        if not _close_outcome_is_hook_safe(close):
            return outcome

        with self._slots_lock:
            slot = self._slots.pop(name, None)
        if slot is not None:
            self._invoke_shutdown(slot)
        return outcome

    def get_slot_info(self, name: str) -> Scheduler:
        with self._slots_lock:
            slot = self._slots.get(name)
        if slot is None:
            raise KeyError(f'Name not registered: {name!r}')
        return slot.scheduler

    def get_local_slot_info(self, name: str) -> tuple[object, Scheduler, CRMContract] | None:
        """Return Python dispatch glue for same-process fast-path calls."""
        with self._slots_lock:
            slot = self._slots.get(name)
        if slot is None:
            return None
        return (
            slot.direct_instance,
            slot.scheduler,
            CRMContract(
                crm_ns=slot.crm_ns,
                crm_name=slot.crm_name,
                crm_ver=slot.crm_ver,
                abi_hash=slot.abi_hash,
                signature_hash=slot.signature_hash,
            ),
        )

    @property
    def names(self) -> list[str]:
        with self._slots_lock:
            return list(self._slots.keys())

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def is_started(self) -> bool:
        return bool(getattr(self._rust_server, 'is_running', False))

    def start(self, timeout: float = 5.0) -> None:
        self._rust_server.start_and_wait(float(timeout))

    def shutdown(
        self,
        *,
        timeout: float = 5.0,
        runtime_session: object | None = None,
        relay_anchor_address: str | None = None,
    ) -> dict[str, Any]:
        with self._slots_lock:
            slots_by_name = dict(self._slots)
            route_names = list(slots_by_name)

        _ensure_no_standalone_relay(runtime_session, relay_anchor_address)
        if runtime_session is None:
            runtime_session = _new_standalone_runtime_session()

        outcome = dict(runtime_session.shutdown(
            self._rust_server,
            route_names=route_names,
            relay_anchor_address=relay_anchor_address,
            timeout_seconds=float(timeout),
        ))

        close_by_route = _close_outcome_by_route(outcome)
        removed_names = [
            name for name in route_names if _close_outcome_is_hook_safe(close_by_route.get(name))
        ]
        removed_slots: list[CRMSlot] = []
        with self._slots_lock:
            for name in removed_names:
                slot = self._slots.pop(name, None)
                if slot is not None:
                    removed_slots.append(slot)

        for slot in removed_slots:
            self._invoke_shutdown(slot)
        return outcome

    # ------------------------------------------------------------------
    # CRM instance helpers
    # ------------------------------------------------------------------

    @staticmethod
    def _create_crm_instance(crm_class: type, crm_instance: object) -> object:
        instance = crm_class()
        instance.resource = crm_instance
        instance.direction = '<-'
        return instance

    @staticmethod
    def _discover_methods(crm_class: type) -> list[str]:
        return rpc_method_names(crm_class)

    @staticmethod
    def _extract_namespace(crm_class: type) -> str:
        return NativeServerBridge._extract_contract_identity(crm_class)[0]

    @staticmethod
    def _extract_contract_identity(crm_class: type) -> tuple[str, str, str]:
        return crm_contract_identity(crm_class)

    @staticmethod
    def _route_contract_hashes(
        crm_ns: str,
        crm_name: str,
        crm_ver: str,
        methods: list[str],
        crm_class: type,
    ) -> tuple[str, str]:
        return crm_route_contract_hashes(crm_ns, crm_name, crm_ver, methods, crm_class)

    # ------------------------------------------------------------------
    # Shutdown callback
    # ------------------------------------------------------------------

    @staticmethod
    def _invoke_shutdown(slot: CRMSlot) -> None:
        sd = slot.shutdown_method
        if sd is None:
            return
        crm = getattr(slot.crm_instance, 'resource', None)
        if crm is None:
            return
        try:
            getattr(crm, sd)()
        except Exception:
            logger.warning(
                'Error invoking @on_shutdown for %s', slot.name, exc_info=True,
            )

    def hold_stats(self) -> dict:
        """Return retained runtime buffer tracking statistics."""
        return dict(self._lease_tracker.stats())

    # ------------------------------------------------------------------
    # Dispatch callable factory
    # ------------------------------------------------------------------

    def _make_dispatcher(
        self, route_name: str, slot: CRMSlot,
    ) -> Callable[[str, int, object, object], object]:
        """Build the Python callable passed into native route registration.

        The callable is invoked from Rust's ``spawn_blocking`` with the GIL
        held.  Signature: ``(route_name, method_idx, shm_buffer, response_allocator)``.
        It reads the request via ``memoryview(shm_buffer)``, resolves the
        method, calls the resource, and returns serialized result data (or
        *None* for empty responses). Rust native code owns the response
        allocation choice for inline, SHM, or chunked transport.
        """
        idx_to_name = slot.method_table._idx_to_name
        dispatch_table = slot._dispatch_table

        lease_tracker = self._lease_tracker
        hold_warn_seconds = self._hold_warn_seconds
        hold_sweep_interval = self._hold_sweep_interval
        hold_dispatch_count = 0

        def dispatch(
            _route_name: str, method_idx: int,
            request_buf: object,
            response_allocator: object,
        ) -> object:
            # 1. Resolve method
            method_name = idx_to_name.get(method_idx)
            if method_name is None:
                raise RuntimeError(
                    f'Unknown method index {method_idx} for route {route_name}',
                )
            entry = dispatch_table.get(method_name)
            if entry is None:
                raise RuntimeError(f'Method not found: {method_name}')
            method, _access, buffer_mode = entry

            # 2. Buffer-mode-aware request handling
            if buffer_mode == 'view':
                # Pass memoryview; _release_fn frees SHM after deserialize
                mv = memoryview(request_buf)
                released = False
                def release_fn():
                    nonlocal released
                    if not released:
                        released = True
                        mv.release()
                        try:
                            request_buf.release()
                        except Exception:
                            pass
                try:
                    result = method(
                        mv,
                        _release_fn=release_fn,
                        _c2_input_buffer_mode=buffer_mode,
                        _c2_output_allocator=response_allocator,
                    )
                finally:
                    if not released:
                        release_fn()
            else:  # borrowed
                mv = memoryview(request_buf)
                released = False

                def release_fn():
                    nonlocal released
                    if not released:
                        released = True
                        mv.release()
                        try:
                            request_buf.release()
                        except Exception:
                            pass

                try:
                    if lease_tracker is not None and hasattr(request_buf, 'track_retained'):
                        request_buf.track_retained(
                            lease_tracker,
                            route_name,
                            method_name,
                            'resource_input',
                        )
                    result = method(
                        mv,
                        _release_fn=release_fn,
                        _c2_input_buffer_mode=buffer_mode,
                        _c2_output_allocator=response_allocator,
                    )
                except Exception:
                    if not released:
                        release_fn()
                    raise
                if buffer_mode == 'borrowed' and not released:
                    release_fn()

                nonlocal hold_dispatch_count
                hold_dispatch_count += 1
                should_sweep_holds = (
                    lease_tracker is not None
                    and hold_dispatch_count % hold_sweep_interval == 0
                )
                if should_sweep_holds:
                    for stale in lease_tracker.sweep_retained(hold_warn_seconds):
                        if stale.get('direction') != 'resource_input':
                            continue
                        logger.warning(
                            "Retained resource input buffer pinned for %.1fs — "
                            "route=%s method=%s storage=%s size=%d bytes. "
                            "Resource may be storing buffer-backed views.",
                            stale['age_seconds'],
                            stale['route_name'],
                            stale['method_name'],
                            stale['storage'],
                            stale['bytes'],
                        )

            # 3. Unpack result
            res_part, err_part = unpack_resource_result(result)
            if err_part:
                raise CrmCallError(err_part)
            if not res_part:
                return None
            return res_part

        return dispatch
