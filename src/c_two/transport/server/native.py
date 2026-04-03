"""Native server bridge — Python wrapper around ``RustServer``.

Preserves the same interface as the Python :class:`Server` for
``registry.py`` compatibility, but delegates transport to the Rust
``c2-server`` crate via PyO3 bindings.

CRM domain logic (ICRM creation, method discovery, dispatch tables,
shutdown callbacks) remains in Python.  Only the UDS accept loop,
frame parsing, heartbeat, and concurrency scheduling move to Rust.
"""
from __future__ import annotations

import inspect
import logging
import threading
from dataclasses import dataclass, field
from typing import Any, Callable

from ...crm.meta import MethodAccess, get_method_access, get_shutdown_method
from ..ipc.frame import IPCConfig
from ..wire import MethodTable
from .scheduler import ConcurrencyConfig, Scheduler
from .reply import unpack_icrm_result

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
    icrm: object
    method_table: MethodTable
    scheduler: Scheduler
    methods: list[str]
    shutdown_method: str | None = None
    _dispatch_table: dict[str, tuple[Any, MethodAccess, str]] = field(
        default_factory=dict, repr=False,
    )

    def build_dispatch_table(self) -> None:
        for name in self.methods:
            if name == self.shutdown_method:
                continue
            method = getattr(self.icrm, name, None)
            if method is not None:
                access = get_method_access(method)
                buffer_mode = getattr(method, '_input_buffer_mode', 'copy')
                self._dispatch_table[name] = (method, access, buffer_mode)


class NativeServerBridge:
    """Drop-in replacement for the Python ``Server`` class.

    Manages CRM slots in Python (ICRM creation, dispatch tables,
    shutdown callbacks) while delegating the IPC transport to the
    Rust ``RustServer`` (c2-server crate).
    """

    def __init__(
        self,
        bind_address: str,
        icrm_class: type | None = None,
        crm_instance: object | None = None,
        ipc_config: IPCConfig | None = None,
        concurrency: ConcurrencyConfig | None = None,
        max_workers: int = 4,
        *,
        name: str | None = None,
    ) -> None:
        self._config = ipc_config or IPCConfig()
        self._address = bind_address
        self._default_concurrency = concurrency or ConcurrencyConfig(
            max_workers=max_workers,
        )

        self._slots: dict[str, CRMSlot] = {}
        self._slots_lock = threading.Lock()
        self._default_name: str | None = None
        self._started = False

        from c_two._native import RustServer

        chunked_threshold = int(
            self._config.max_payload_size * self._config.chunk_threshold_ratio,
        )
        self._rust_server = RustServer(
            address=bind_address,
            max_frame_size=self._config.max_frame_size,
            max_payload_size=self._config.max_payload_size,
            max_pool_segments=self._config.max_pool_segments,
            segment_size=self._config.pool_segment_size,
            chunked_threshold=chunked_threshold,
            heartbeat_interval=self._config.heartbeat_interval,
            heartbeat_timeout=self._config.heartbeat_timeout,
            shm_threshold=self._config.shm_threshold,
        )

        # Register initial CRM if provided (compat with old Server constructor).
        if icrm_class is not None and crm_instance is not None:
            self.register_crm(
                icrm_class, crm_instance, concurrency, name=name,
            )

    # ------------------------------------------------------------------
    # CRM registration
    # ------------------------------------------------------------------

    def register_crm(
        self,
        icrm_class: type,
        crm_instance: object,
        concurrency: ConcurrencyConfig | None = None,
        *,
        name: str | None = None,
    ) -> str:
        routing_name = (
            name if name is not None else self._extract_namespace(icrm_class)
        )
        icrm = self._create_icrm(icrm_class, crm_instance)
        methods = self._discover_methods(icrm_class)
        cc_config = concurrency or self._default_concurrency
        scheduler = Scheduler(cc_config)
        sd_method = get_shutdown_method(icrm_class)

        if sd_method is not None:
            methods = [m for m in methods if m != sd_method]
        method_table = MethodTable.from_methods(methods)

        slot = CRMSlot(
            name=routing_name,
            icrm=icrm,
            method_table=method_table,
            scheduler=scheduler,
            methods=methods,
            shutdown_method=sd_method,
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

        with self._slots_lock:
            if routing_name in self._slots:
                scheduler.shutdown()
                raise ValueError(f'Name already registered: {routing_name!r}')
            self._slots[routing_name] = slot
            if self._default_name is None:
                self._default_name = routing_name

        self._rust_server.register_route(
            routing_name, dispatcher, methods, access_map,
        )
        return routing_name

    def unregister_crm(self, name: str) -> None:
        with self._slots_lock:
            slot = self._slots.pop(name, None)
            if slot is None:
                raise KeyError(f'Name not registered: {name!r}')
            if self._default_name == name:
                self._default_name = next(iter(self._slots), None)
        self._invoke_shutdown(slot)
        slot.scheduler.shutdown()
        self._rust_server.unregister_route(name)

    def get_slot_info(
        self, name: str,
    ) -> tuple[Scheduler, dict[str, MethodAccess]]:
        with self._slots_lock:
            slot = self._slots.get(name)
        if slot is None:
            raise KeyError(f'Name not registered: {name!r}')
        access_map = {
            mname: access
            for mname, (_, access, _bm) in slot._dispatch_table.items()
        }
        return slot.scheduler, access_map

    @property
    def names(self) -> list[str]:
        with self._slots_lock:
            return list(self._slots.keys())

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def is_started(self) -> bool:
        return self._started

    def start(self, timeout: float = 5.0) -> None:
        self._rust_server.start()
        # Wait for the UDS socket to be created by the Rust server.
        import os
        import time

        socket_path = self._rust_server.socket_path
        deadline = time.monotonic() + timeout
        while not os.path.exists(socket_path):
            if time.monotonic() > deadline:
                raise RuntimeError(
                    f'RustServer failed to start within {timeout}s',
                )
            time.sleep(0.01)
        self._started = True

    def shutdown(self) -> None:
        with self._slots_lock:
            slots_snapshot = list(self._slots.values())
        for slot in slots_snapshot:
            self._invoke_shutdown(slot)

        if self._started:
            try:
                self._rust_server.shutdown()
            except Exception:
                logger.warning('Error shutting down RustServer', exc_info=True)
            self._started = False

        with self._slots_lock:
            for slot in self._slots.values():
                slot.scheduler.shutdown()

    # ------------------------------------------------------------------
    # ICRM helpers
    # ------------------------------------------------------------------

    @staticmethod
    def _create_icrm(icrm_class: type, crm_instance: object) -> object:
        icrm = icrm_class()
        icrm.crm = crm_instance
        icrm.direction = '<-'
        return icrm

    @staticmethod
    def _discover_methods(icrm_class: type) -> list[str]:
        return sorted(
            name
            for name, _ in inspect.getmembers(
                icrm_class, predicate=inspect.isfunction,
            )
            if not name.startswith('_')
        )

    @staticmethod
    def _extract_namespace(icrm_class: type) -> str:
        tag = getattr(icrm_class, '__tag__', '')
        return tag.split('/')[0] if tag else ''

    # ------------------------------------------------------------------
    # Shutdown callback
    # ------------------------------------------------------------------

    @staticmethod
    def _invoke_shutdown(slot: CRMSlot) -> None:
        sd = slot.shutdown_method
        if sd is None:
            return
        crm = getattr(slot.icrm, 'crm', None)
        if crm is None:
            return
        try:
            getattr(crm, sd)()
        except Exception:
            logger.warning(
                'Error invoking @on_shutdown for %s', slot.name, exc_info=True,
            )

    # ------------------------------------------------------------------
    # Dispatch callable factory
    # ------------------------------------------------------------------

    def _make_dispatcher(
        self, route_name: str, slot: CRMSlot,
    ) -> Callable[[str, int, object, object], object]:
        """Build the Python callable passed to ``RustServer.register_route()``.

        The callable is invoked from Rust's ``spawn_blocking`` with the GIL
        held.  Signature: ``(route_name, method_idx, shm_buffer, response_pool)``.
        It reads the request via ``memoryview(shm_buffer)``, resolves the
        method, calls the ICRM, and returns result bytes (or *None* for empty
        responses).  For large responses (> shm_threshold), allocates from
        ``response_pool`` SHM and returns a ``(seg_idx, offset, data_size,
        is_dedicated)`` tuple — Rust sends the buddy frame directly.
        """
        idx_to_name = slot.method_table._idx_to_name
        dispatch_table = slot._dispatch_table
        shm_threshold = self._config.shm_threshold

        def dispatch(
            _route_name: str, method_idx: int,
            request_buf: object, response_pool: object,
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
            if buffer_mode == 'copy':
                # Materialize to bytes, release SHM immediately
                try:
                    mv = memoryview(request_buf)
                    payload = bytes(mv)
                    mv.release()
                finally:
                    try:
                        request_buf.release()
                    except Exception:
                        pass
                result = method(payload)
            elif buffer_mode == 'view':
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
                    result = method(mv, _release_fn=release_fn)
                finally:
                    if not released:
                        release_fn()
            else:  # hold
                # Pass memoryview directly; RAII handles lifetime
                mv = memoryview(request_buf)
                result = method(mv)

            # 3. Unpack result
            res_part, err_part = unpack_icrm_result(result)
            if err_part:
                raise CrmCallError(err_part)
            if not res_part:
                return None

            # 4. For large responses, write to response pool SHM
            if response_pool is not None and len(res_part) > shm_threshold:
                try:
                    alloc = response_pool.alloc(len(res_part))
                    response_pool.write(alloc, res_part)
                    seg_idx = int(alloc.seg_idx) & 0xFFFF
                    return (seg_idx, alloc.offset, len(res_part), alloc.is_dedicated)
                except Exception:
                    pass

            return res_part

        return dispatch
