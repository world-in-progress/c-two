"""Asyncio-based IPC server with multi-CRM hosting.

- **No EventQueue**: direct CRM dispatch via ``ThreadPoolExecutor``.
- **Handshake**: capability negotiation + method index exchange.
- **Control-plane routing**: CRM routing name + method index in UDS
  inline frame, pure payload in buddy SHM.
- **Multi-CRM**: hosts multiple CRM instances under distinct routing
  names (not tied to ICRM namespace from ``__tag__``).
"""
from __future__ import annotations

import asyncio
import ctypes
import inspect
import logging
import os
import struct
import threading
import time
from pathlib import Path
from typing import Any

from ... import error
from ...crm.meta import MethodAccess, get_method_access, get_shutdown_method
from ..ipc.msg_type import (
    MsgType,
    PONG_BYTES,
    SHUTDOWN_ACK_BYTES,
    DISCONNECT_ACK_BYTES,
    _SIGNAL_TYPES,
)
from ..protocol import FLAG_SIGNAL, FLAG_CALL, FLAG_CHUNKED
from ..ipc.frame import (
    IPCConfig,
    FRAME_STRUCT,
    FLAG_RESPONSE,
    FLAG_CTRL,
    encode_frame,
)
from ..ipc.shm_frame import (
    FLAG_BUDDY,
    BUDDY_PAYLOAD_STRUCT,
    decode_buddy_payload,
)
from ..wire import (
    MethodTable,
    decode_call_control,
    decode_chunk_header,
    CHUNK_HEADER_SIZE,
    encode_error_reply_frame,
    U16_LE,
)
from .scheduler import Scheduler, ConcurrencyConfig

from .connection import Connection, CRMSlot, parse_call_inline
from .reply import unpack_icrm_result, wrap_error, send_reply
from .dispatcher import FastDispatcher
from ...mem import cleanup_stale_shm, ChunkAssembler as RustChunkAssembler, MemPool as RustMemPool, PoolConfig as RustPoolConfig
from .heartbeat import run_heartbeat
from .handshake import handle_handshake, FLAG_HANDSHAKE

logger = logging.getLogger(__name__)

_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')


# ---------------------------------------------------------------------------
# Server
# ---------------------------------------------------------------------------

class Server:
    """IPC server with control-plane routing.

    Supports hosting **multiple CRM resources** under distinct routing
    names.  Each name has its own ICRM instance, method table, and
    scheduler.

    Handles frames with control-plane method index + pure payload.
    Frames carry the routing name in the control plane.

    Parameters
    ----------
    bind_address:
        ``ipc://<region_id>`` — the Unix socket will be created at
        ``/tmp/c_two_ipc/<region_id>.sock``.
    icrm_class:
        Optional ``@cc.icrm``-decorated interface class to register at
        construction time.  Omit to register CRMs later via
        :meth:`register_crm`.
    crm_instance:
        The concrete CRM object (required if *icrm_class* is provided).
    ipc_config:
        Optional transport configuration (SHM threshold, pool sizes, …).
    concurrency:
        Default concurrency config applied to CRMs without explicit config.
    max_workers:
        Shorthand for ``ConcurrencyConfig(max_workers=N)`` when
        *concurrency* is not given.
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
    ):
        self._config = ipc_config or IPCConfig()
        self._address = bind_address
        region_id = bind_address.replace('ipc://', '')
        self._socket_path = str(Path(_IPC_SOCK_DIR) / f'{region_id}.sock')

        # Multi-CRM slots: name → CRMSlot.
        self._slots: dict[str, CRMSlot] = {}
        self._slots_lock = threading.Lock()
        self._slots_generation: int = 0
        self._default_name: str | None = None
        self._default_concurrency = concurrency or ConcurrencyConfig(
            max_workers=max_workers,
        )

        # Register initial CRM if provided.
        if icrm_class is not None and crm_instance is not None:
            self.register_crm(
                icrm_class, crm_instance, self._default_concurrency,
                name=name,
            )
        elif icrm_class is not None or crm_instance is not None:
            raise ValueError(
                'Both icrm_class and crm_instance must be provided, or neither',
            )

        # asyncio event loop (runs in background thread).
        self._loop: asyncio.AbstractEventLoop | None = None
        self._loop_thread: threading.Thread | None = None
        self._server: asyncio.Server | None = None
        self._started = threading.Event()
        self._shutdown_event: asyncio.Event | None = None

        # Dedicated reassembly pool for Rust ChunkAssembler.
        self._reassembly_pool = RustMemPool(RustPoolConfig(
            segment_size=self._config.pool_segment_size,
            min_block_size=4096,
            max_segments=8,
            max_dedicated_segments=4,
        ))

        # Connection tracking.
        self._conn_counter = 0
        self._client_tasks: list[asyncio.Task] = []

    def is_started(self) -> bool:
        """Return ``True`` if the server has started and is accepting connections."""
        return self._started.is_set()

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
        """Register a CRM under an explicit *name* (routing key).

        Parameters
        ----------
        name:
            Unique routing key.  If ``None``, falls back to the ICRM
            namespace extracted from ``__tag__``.

        Returns the routing name.  Raises ``ValueError`` if the name is
        already registered.
        """
        routing_name = name if name is not None else self._extract_namespace(icrm_class)
        icrm = self._create_icrm(icrm_class, crm_instance)
        methods = self._discover_methods(icrm_class)
        cc_config = concurrency or self._default_concurrency
        scheduler = Scheduler(cc_config)
        sd_method = get_shutdown_method(icrm_class)

        # Exclude shutdown method from RPC-visible method list.
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
        with self._slots_lock:
            if routing_name in self._slots:
                scheduler.shutdown()
                raise ValueError(f'Name already registered: {routing_name!r}')
            self._slots[routing_name] = slot
            self._slots_generation += 1
            if self._default_name is None:
                self._default_name = routing_name
        return routing_name

    def unregister_crm(self, name: str) -> None:
        """Unregister a CRM by its routing name.

        Invokes the ``@cc.on_shutdown`` method (if declared) on the
        underlying CRM instance before shutting down the scheduler.
        """
        with self._slots_lock:
            slot = self._slots.pop(name, None)
            if slot is None:
                raise KeyError(f'Name not registered: {name!r}')
            self._slots_generation += 1
            if self._default_name == name:
                self._default_name = next(iter(self._slots), None)
        self._invoke_shutdown(slot)
        slot.scheduler.shutdown()

    def get_slot_info(
        self, name: str,
    ) -> tuple[Scheduler, dict[str, MethodAccess]]:
        """Return the Scheduler and method access map for a registered CRM.

        Used by the registry to inject concurrency control into
        thread-local proxies.
        """
        with self._slots_lock:
            slot = self._slots.get(name)
        if slot is None:
            raise KeyError(f'Name not registered: {name!r}')
        access_map = {
            mname: access for mname, (_, access) in slot._dispatch_table.items()
        }
        return slot.scheduler, access_map

    @property
    def names(self) -> list[str]:
        """Return list of registered routing names."""
        with self._slots_lock:
            return list(self._slots.keys())

    # ------------------------------------------------------------------
    # ICRM helpers
    # ------------------------------------------------------------------

    @staticmethod
    def _create_icrm(icrm_class: type, crm_instance: object) -> object:
        """Instantiate ICRM in server direction (``'<-'``)."""
        icrm = icrm_class()
        icrm.crm = crm_instance
        icrm.direction = '<-'
        return icrm

    @staticmethod
    def _discover_methods(icrm_class: type) -> list[str]:
        """Return sorted list of public method names declared in ICRM."""
        return sorted(
            name
            for name, _ in inspect.getmembers(icrm_class, predicate=inspect.isfunction)
            if not name.startswith('_')
        )

    @staticmethod
    def _extract_namespace(icrm_class: type) -> str:
        tag = getattr(icrm_class, '__tag__', '')
        return tag.split('/')[0] if tag else ''

    def _resolve_slot(self, name: str) -> CRMSlot | None:
        """Look up a CRM slot by routing name (empty → default).

        Acquires ``_slots_lock`` for the dict read to be safe under
        free-threaded Python (3.13t / 3.14t) where dict operations are
        not inherently atomic.
        """
        with self._slots_lock:
            slots = self._slots
            if name:
                return slots.get(name)
            dn = self._default_name
            if dn is not None:
                return slots.get(dn)
            return None

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def start(self, timeout: float = 5.0) -> None:
        """Start the server in a background thread.  Blocks until ready."""
        self._loop_thread = threading.Thread(
            target=self._run_loop,
            name=f'c2-srv-{self._address}',
            daemon=True,
        )
        self._loop_thread.start()
        if not self._started.wait(timeout):
            raise RuntimeError('Server failed to start within timeout')

    def _run_loop(self) -> None:
        self._loop = asyncio.new_event_loop()
        asyncio.set_event_loop(self._loop)
        try:
            self._loop.run_until_complete(self._async_main())
        finally:
            self._loop.close()

    async def _async_main(self) -> None:
        self._shutdown_event = asyncio.Event()
        self._dispatcher = FastDispatcher(asyncio.get_running_loop())

        os.makedirs(os.path.dirname(self._socket_path), exist_ok=True)
        if os.path.exists(self._socket_path):
            os.unlink(self._socket_path)

        self._server = await asyncio.start_unix_server(
            self._handle_client,
            path=self._socket_path,
        )
        # Restrict socket access to the owning user (defense-in-depth
        # for multi-user HPC nodes where /tmp is shared).
        try:
            os.chmod(self._socket_path, 0o600)
        except OSError:
            pass
        self._started.set()
        logger.debug('Server listening on %s', self._socket_path)

        await self._shutdown_event.wait()

        # Graceful shutdown.
        for task in list(self._client_tasks):
            task.cancel()
        if self._client_tasks:
            await asyncio.gather(*self._client_tasks, return_exceptions=True)
        self._dispatcher.shutdown()
        self._server.close()
        await self._server.wait_closed()
        with self._slots_lock:
            slots = list(self._slots.values())
        for slot in slots:
            slot.scheduler.shutdown()

    def shutdown(self) -> None:
        """Signal the server to shut down and wait for the loop thread.

        Invokes ``@cc.on_shutdown`` methods on all registered CRMs
        before tearing down the event loop.  Safe to call multiple
        times — subsequent calls are no-ops.
        """
        # Invoke shutdown callbacks before stopping the loop.
        with self._slots_lock:
            slots_snapshot = list(self._slots.values())
        for slot in slots_snapshot:
            self._invoke_shutdown(slot)

        if self._loop is not None and self._shutdown_event is not None:
            try:
                self._loop.call_soon_threadsafe(self._shutdown_event.set)
            except RuntimeError:
                pass  # Event loop already closed
        if self._loop_thread is not None:
            self._loop_thread.join(timeout=5.0)
            self._loop_thread = None
        try:
            os.unlink(self._socket_path)
        except OSError:
            pass

        # Best-effort cleanup of SHM segments owned by dead processes.
        try:
            cleaned = cleanup_stale_shm('cc')
            if cleaned > 0:
                logger.info('Cleaned %d stale SHM segments', cleaned)
        except Exception:
            logger.debug('SHM cleanup failed', exc_info=True)

    # ------------------------------------------------------------------
    # Shutdown callback helper
    # ------------------------------------------------------------------

    @staticmethod
    def _invoke_shutdown(slot: CRMSlot) -> None:
        """Best-effort invocation of a CRM's ``@cc.on_shutdown`` method."""
        sd = slot.shutdown_method
        if sd is None:
            return
        # slot.icrm is the ICRM *wrapper* (created by _create_icrm), not
        # the CRM instance itself.  The actual CRM is stored as
        # slot.icrm.crm — the @on_shutdown method is defined on the CRM
        # class, so we resolve it from the underlying instance.
        crm = getattr(slot.icrm, 'crm', None)
        if crm is None:
            return
        try:
            getattr(crm, sd)()
        except Exception:
            logger.warning(
                'Error in @on_shutdown method %s.%s for CRM %r',
                type(crm).__name__, sd, slot.name,
                exc_info=True,
            )

    # ------------------------------------------------------------------
    # Client handler
    # ------------------------------------------------------------------

    async def _handle_client(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
    ) -> None:
        self._conn_counter += 1
        conn_id = self._conn_counter
        conn = Connection(conn_id=conn_id, writer=writer, config=self._config)
        conn.init_flight_tracking(asyncio.get_running_loop())
        task = asyncio.current_task()
        self._client_tasks.append(task)

        # Pipelined dispatch: read next frame while previous dispatches
        # are still executing in the thread pool.
        pending: set[asyncio.Task] = set()
        chunk_assemblers: dict[int, tuple[RustChunkAssembler, float]] = {}

        try:
            max_frame = self._config.max_frame_size
            slots = self._slots
            default_name = self._default_name
            dispatcher = self._dispatcher
            dispatch_cache: dict[tuple[bytes, int], tuple] = {}
            cache_gen = self._slots_generation
            frame_count = 0

            # Start heartbeat probe for this connection.
            heartbeat_task: asyncio.Task | None = None
            if self._config.heartbeat_interval > 0:
                heartbeat_task = asyncio.create_task(run_heartbeat(conn))

            while True:
                header = await reader.readexactly(16)
                total_len, request_id, flags = FRAME_STRUCT.unpack(header)

                if total_len < 12:
                    raise ValueError(f'Frame too small: {total_len}')
                if total_len > max_frame:
                    raise ValueError(f'Frame too large: {total_len}')

                payload_len = total_len - 12
                payload = (
                    await reader.readexactly(payload_len)
                    if payload_len > 0
                    else b''
                )

                conn.touch()

                # Handshake & control: must complete before reading more.
                if flags & FLAG_HANDSHAKE:
                    with self._slots_lock:
                        slots_snap = list(self._slots.values())
                    await handle_handshake(conn, payload, writer, slots_snap, self._config)
                    continue
                if flags & FLAG_CTRL:
                    continue

                # Signal handling (frame-level, requires FLAG_SIGNAL)
                if flags & FLAG_SIGNAL and len(payload) == 1 and payload[0] in _SIGNAL_TYPES:
                    sig_type = payload[0]
                    if sig_type == MsgType.PING:
                        writer.write(encode_frame(request_id, FLAG_RESPONSE | FLAG_SIGNAL, PONG_BYTES))
                    elif sig_type == MsgType.SHUTDOWN_CLIENT:
                        if self._shutdown_event:
                            self._shutdown_event.set()
                        writer.write(encode_frame(request_id, FLAG_RESPONSE | FLAG_SIGNAL, SHUTDOWN_ACK_BYTES))
                        return  # close connection after shutdown ack
                    elif sig_type == MsgType.DISCONNECT:
                        writer.write(encode_frame(request_id, FLAG_RESPONSE | FLAG_SIGNAL, DISCONNECT_ACK_BYTES))
                        logger.debug('Conn %d: graceful disconnect', conn.conn_id)
                        return  # close this connection only
                    continue

                # Periodic GC for stale chunk assemblers.
                frame_count += 1
                if frame_count % self._config.chunk_gc_interval == 0 and chunk_assemblers:
                    now = time.monotonic()
                    expired = [
                        rid for rid, (asm, created) in chunk_assemblers.items()
                        if now - created > self._config.chunk_assembler_timeout
                    ]
                    for rid in expired:
                        asm, _ = chunk_assemblers.pop(rid)
                        logger.warning(
                            'Conn %d: GC stale chunk assembler rid=%d (%d chunks)',
                            conn.conn_id, rid, asm.received,
                        )
                        asm.abort()

                # Chunked frame: accumulate in assembler, dispatch on completion.
                if flags & FLAG_CHUNKED:
                    t = asyncio.create_task(
                        self._process_chunked_frame(
                            conn, request_id, flags, payload, writer,
                            chunk_assemblers,
                        ),
                    )
                    pending.add(t)
                    t.add_done_callback(pending.discard)
                    continue

                is_routed = bool(flags & FLAG_CALL)
                is_buddy = bool(flags & FLAG_BUDDY)

                # Fast inline path for non-buddy calls:
                # parse + dispatch table lookup + submit all without Task.
                if is_routed and not is_buddy:
                    try:
                        route_key, method_idx, args_start = parse_call_inline(payload)
                    except (struct.error, IndexError) as exc:
                        writer.write(encode_error_reply_frame(
                            request_id,
                            f'Malformed call control: {exc}'.encode('utf-8'),
                        ))
                        continue
                    args_bytes = payload[args_start:] if args_start < len(payload) else b''

                    # Invalidate dispatch cache when CRM registrations change.
                    cur_gen = self._slots_generation
                    if cur_gen != cache_gen:
                        dispatch_cache.clear()
                        cache_gen = cur_gen
                        with self._slots_lock:
                            slots = self._slots.copy()
                            default_name = self._default_name

                    # Combined cache: (route_bytes, method_idx) → dispatch tuple
                    cache_key = (bytes(route_key), method_idx)
                    cached = dispatch_cache.get(cache_key)
                    if cached is not None:
                        exec_fast, method, access = cached
                        conn.flight_inc()
                        if access is MethodAccess.WRITE:
                            barrier = asyncio.Event()
                            dispatcher.submit_inline(
                                exec_fast, method, args_bytes, access,
                                request_id, writer, barrier, conn.flight_dec,
                            )
                            await barrier.wait()
                        else:
                            dispatcher.submit_inline(
                                exec_fast, method, args_bytes, access,
                                request_id, writer, None, conn.flight_dec,
                            )
                        continue

                    route_name = route_key.decode('utf-8') if route_key else ''
                    slot = (slots.get(route_name) if route_name
                            else slots.get(default_name or ''))
                    if slot is not None:
                        method_name = slot.method_table._idx_to_name.get(method_idx)
                        if method_name is not None:
                            entry = slot._dispatch_table.get(method_name)
                            if entry is not None:
                                method, access = entry
                                dispatch_cache[cache_key] = (slot.scheduler.execute_fast, method, access)
                                conn.flight_inc()
                                if access is MethodAccess.WRITE:
                                    barrier = asyncio.Event()
                                    dispatcher.submit_inline(
                                        slot.scheduler.execute_fast, method,
                                        args_bytes, access, request_id, writer,
                                        barrier, conn.flight_dec,
                                    )
                                    await barrier.wait()
                                else:
                                    dispatcher.submit_inline(
                                        slot.scheduler.execute_fast, method,
                                        args_bytes, access, request_id, writer,
                                        None, conn.flight_dec,
                                    )
                                continue

                # Fallback: full handler via Task for buddy or error paths.
                if is_routed:
                    t = asyncio.create_task(
                        self._handle_call(
                            conn, request_id, payload, is_buddy, writer,
                        ),
                    )
                else:
                    continue  # Non-routed, non-signal frame — ignore
                pending.add(t)
                t.add_done_callback(pending.discard)

        except asyncio.IncompleteReadError:
            pass
        except asyncio.CancelledError:
            pass
        except Exception:
            logger.debug('Conn %d: handler error', conn.conn_id, exc_info=True)
        finally:
            # Cancel heartbeat probe.
            heartbeat_expired = False
            if heartbeat_task is not None:
                heartbeat_task.cancel()
                try:
                    await heartbeat_task
                except asyncio.CancelledError:
                    pass
                if heartbeat_task.done() and not heartbeat_task.cancelled():
                    heartbeat_expired = True

            # Drain in-flight tasks before tearing down the connection.
            if pending:
                await asyncio.gather(*pending, return_exceptions=True)
            # Clean up stale chunk assemblers.
            for asm, _ in chunk_assemblers.values():
                asm.abort()
            chunk_assemblers.clear()
            # Wait for submit_inline requests still executing in worker
            # threads.  Without this, writer.close() could discard replies
            # for requests that have already completed on the CRM side.
            await conn.wait_idle()
            conn.cleanup()
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass
            if task in self._client_tasks:
                self._client_tasks.remove(task)

            if heartbeat_expired:
                logger.warning('Conn %d: disconnected (heartbeat timeout)', conn.conn_id)
            else:
                logger.debug('Conn %d: disconnected', conn.conn_id)

    # ------------------------------------------------------------------
    # Chunked frame handling
    # ------------------------------------------------------------------

    async def _process_chunked_frame(
        self,
        conn: Connection,
        request_id: int,
        flags: int,
        payload: bytes,
        writer: asyncio.StreamWriter,
        assemblers: dict[int, tuple[RustChunkAssembler, float]],
    ) -> None:
        """Process a single chunked frame, dispatching when all chunks arrive."""
        is_buddy = bool(flags & FLAG_BUDDY)

        if is_buddy:
            try:
                seg_idx, data_offset, data_size, is_dedicated, free_offset, free_size = (
                    decode_buddy_payload(payload)
                )
            except (struct.error, Exception) as exc:
                logger.warning('Conn %d: malformed chunked buddy payload: %s',
                               conn.conn_id, exc)
                return
            if conn.buddy_pool is None:
                return
            seg_mv = conn.seg_views.get(seg_idx)
            if seg_mv is None:
                from .handshake import lazy_open_peer_seg
                seg_mv = lazy_open_peer_seg(conn, seg_idx)
                if seg_mv is None:
                    return
            if data_offset + data_size > len(seg_mv):
                logger.warning('Conn %d: chunked buddy OOB', conn.conn_id)
                return
            chunk_data = bytes(seg_mv[data_offset:data_offset + data_size])
            try:
                conn.buddy_pool.free_at(seg_idx, free_offset, free_size, is_dedicated)
            except Exception:
                logger.warning('Conn %d: failed to free chunked buddy block',
                               conn.conn_id, exc_info=True)
            ctrl_off = BUDDY_PAYLOAD_STRUCT.size
        else:
            ctrl_off = 0

        # Parse chunk header.
        try:
            chunk_idx, total_chunks, ch_consumed = decode_chunk_header(payload, ctrl_off)
        except (ValueError, struct.error) as exc:
            logger.warning('Conn %d: malformed chunk header: %s', conn.conn_id, exc)
            return
        ctrl_off += ch_consumed

        if chunk_idx == 0:
            # First chunk: parse call control.
            try:
                route_name, method_idx, cc_consumed = decode_call_control(payload, ctrl_off)
            except (ValueError, struct.error) as exc:
                logger.warning('Conn %d: malformed chunked call control: %s',
                               conn.conn_id, exc)
                return
            ctrl_off += cc_consumed

            if not is_buddy:
                chunk_data = bytes(payload[ctrl_off:])

            chunk_size = self._config.pool_segment_size // 2
            try:
                asm = RustChunkAssembler(self._reassembly_pool, total_chunks, chunk_size)
                asm.route_name = route_name
                asm.method_idx = method_idx
            except Exception as exc:
                logger.error('Conn %d: failed to create chunk assembler: %s',
                             conn.conn_id, exc)
                writer.write(encode_error_reply_frame(
                    request_id,
                    f'Failed to allocate reassembly buffer: {exc}'.encode('utf-8'),
                ))
                await writer.drain()
                return
            assemblers[request_id] = (asm, time.monotonic())
        else:
            entry = assemblers.get(request_id)
            if entry is None:
                logger.warning('Conn %d: orphan chunk (rid=%d, idx=%d)',
                               conn.conn_id, request_id, chunk_idx)
                return
            asm = entry[0]
            if not is_buddy:
                chunk_data = bytes(payload[ctrl_off:])

        complete = asm.feed_chunk(chunk_idx, chunk_data)

        if not complete:
            return

        # All chunks received — capture routing info before finish() consumes the assembler.
        route_name = asm.route_name
        method_idx = asm.method_idx

        mem_handle = asm.finish()
        del assemblers[request_id]
        try:
            addr, length = mem_handle.buffer_info()
            args_mv = memoryview((ctypes.c_char * length).from_address(addr)).cast('B')
            args_bytes = bytes(args_mv)
        finally:
            mem_handle.release()

        # Resolve slot.
        slot = self._resolve_slot(route_name)
        if slot is None:
            logger.warning('Conn %d: unknown route %r in chunked call',
                           conn.conn_id, route_name)
            writer.write(encode_error_reply_frame(
                request_id,
                f'Unknown route name: {route_name}'.encode('utf-8'),
            ))
            await writer.drain()
            return

        method_name = slot.method_table._idx_to_name.get(method_idx)
        if method_name is None:
            logger.warning('Conn %d: unknown method idx %d for route %r',
                           conn.conn_id, method_idx, route_name)
            writer.write(encode_error_reply_frame(
                request_id,
                f'Unknown method index {method_idx}'.encode('utf-8'),
            ))
            await writer.drain()
            return

        result_bytes, err_bytes = await self._dispatch(slot, method_name, args_bytes)
        await send_reply(conn, request_id, result_bytes, err_bytes, writer, self._config)

    # ------------------------------------------------------------------
    # Call handling (control-plane routing)
    # ------------------------------------------------------------------

    async def _handle_call(
        self,
        conn: Connection,
        request_id: int,
        payload: bytes,
        is_buddy: bool,
        writer: asyncio.StreamWriter,
    ) -> None:
        if is_buddy:
            route_name, method_idx, args_bytes = self._resolve_buddy(conn, payload)
            if args_bytes is None:
                return
        else:
            if not payload:
                return
            try:
                route_key, method_idx, args_start = parse_call_inline(payload)
            except (struct.error, IndexError, UnicodeDecodeError) as exc:
                logger.warning('Conn %d: malformed call control: %s',
                               conn.conn_id, exc)
                writer.write(encode_error_reply_frame(
                    request_id, f'Malformed call control: {exc}'.encode('utf-8'),
                ))
                await writer.drain()
                return
            route_name = route_key.decode('utf-8') if route_key else ''
            args_bytes = payload[args_start:] if args_start < len(payload) else b''

        # Inline slot + method resolve.
        slot = self._resolve_slot(route_name)
        if slot is None:
            logger.warning('Unknown route name %r', route_name)
            writer.write(encode_error_reply_frame(
                request_id, f'Unknown route name: {route_name}'.encode('utf-8'),
            ))
            await writer.drain()
            return

        method_name = slot.method_table._idx_to_name.get(method_idx)
        if method_name is None:
            logger.warning('Unknown method index %d for route %r', method_idx, route_name)
            writer.write(encode_error_reply_frame(
                request_id,
                f'Unknown method index {method_idx} for route {route_name!r}'.encode('utf-8'),
            ))
            await writer.drain()
            return

        # Inline dispatch — avoids _dispatch() + _unpack_icrm_result() calls.
        entry = slot._dispatch_table.get(method_name)
        if entry is None:
            err = error.CRMServerError(f'Method not found: {method_name}')
            try:
                err_bytes = err.serialize()
            except Exception:
                err_bytes = str(err).encode('utf-8')
            writer.write(encode_error_reply_frame(request_id, err_bytes))
            await writer.drain()
            return

        method, access = entry

        # For non-buddy calls with no SHM pool, fire-and-forget:
        # the worker thread builds the reply frame and writes directly,
        # avoiding Future creation + asyncio task resume overhead.
        if not is_buddy and (conn.buddy_pool is None or not conn.buddy_pool):
            self._dispatcher.submit_inline(
                slot.scheduler.execute_fast, method, args_bytes, access,
                request_id, writer,
            )
            return

        try:
            future = self._loop.create_future()
            self._dispatcher.submit(slot.scheduler.execute_fast, method, args_bytes, access, future)
            result = await future
        except Exception as exc:
            err_bytes = wrap_error(exc)[1]
            writer.write(encode_error_reply_frame(request_id, err_bytes))
            await writer.drain()
            return

        # Unpack ICRM result and send reply.
        res_part, err_part = unpack_icrm_result(result)

        await send_reply(conn, request_id, res_part, err_part, writer, self._config)

    def _resolve_buddy(
        self, conn: Connection, payload: bytes,
    ) -> tuple[str, int, bytes | None]:
        """Extract route_name + method_idx + args from a buddy frame."""
        try:
            seg_idx, data_offset, data_size, is_dedicated, free_offset, free_size = (
                decode_buddy_payload(payload)
            )
        except (struct.error, Exception) as exc:
            logger.warning('Conn %d: malformed buddy payload: %s',
                           conn.conn_id, exc)
            return '', -1, None
        if conn.buddy_pool is None:
            return '', -1, None
        seg_mv = conn.seg_views.get(seg_idx)
        if seg_mv is None:
            from .handshake import lazy_open_peer_seg
            seg_mv = lazy_open_peer_seg(conn, seg_idx)
            if seg_mv is None:
                return '', -1, None

        ctrl_offset = BUDDY_PAYLOAD_STRUCT.size
        try:
            route_name, method_idx, _consumed = decode_call_control(payload, ctrl_offset)
        except (ValueError, struct.error) as exc:
            logger.warning('Conn %d: malformed buddy call control: %s',
                           conn.conn_id, exc)
            return '', -1, None

        if data_offset + data_size > len(seg_mv):
            logger.warning('Conn %d: buddy OOB: offset=%d + size=%d > seg_len=%d',
                           conn.conn_id, data_offset, data_size, len(seg_mv))
            return '', -1, None
        args_bytes = bytes(seg_mv[data_offset : data_offset + data_size])

        try:
            conn.buddy_pool.free_at(seg_idx, free_offset, free_size, is_dedicated)
        except Exception:
            logger.warning('Conn %d: failed to free request block', conn.conn_id, exc_info=True)

        return route_name, method_idx, args_bytes

    # ------------------------------------------------------------------
    # CRM dispatch
    # ------------------------------------------------------------------

    async def _dispatch(
        self, slot: CRMSlot, method_name: str, args_bytes: bytes,
    ) -> tuple[bytes, bytes]:
        """Execute a CRM method via the slot's scheduler (read/write aware).

        Returns ``(result_bytes, error_bytes)``.  Exactly one is non-empty.
        """
        entry = slot._dispatch_table.get(method_name)
        if entry is None:
            err = error.CRMServerError(f'Method not found: {method_name}')
            try:
                return b'', err.serialize()
            except Exception:
                return b'', str(err).encode('utf-8')

        method, access = entry
        try:
            future = self._loop.create_future()
            self._dispatcher.submit(slot.scheduler.execute_fast, method, args_bytes, access, future)
            result = await future
        except Exception as exc:
            return wrap_error(exc)

        return unpack_icrm_result(result)

