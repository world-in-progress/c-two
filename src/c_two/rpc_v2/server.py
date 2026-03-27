"""Server v2 — asyncio-based IPC v3 server with wire v2 support.

Compared to :class:`IPCv3Server`:

- **No EventQueue**: direct CRM dispatch via ``ThreadPoolExecutor``.
- **Handshake v5**: capability negotiation + method index exchange.
- **Wire v2 frames**: control-plane routing (CRM routing name + method
  index in UDS inline frame, pure payload in buddy SHM).
- **Backward compatible**: also handles v4 handshake and v1 wire frames.
- **Multi-CRM**: hosts multiple CRM instances under distinct routing
  names (not tied to ICRM namespace from ``__tag__``).
"""
from __future__ import annotations

import asyncio
import ctypes
import inspect
import logging
import os
import queue
import re
import struct
import threading
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .. import error
from ..crm.meta import MethodAccess, get_method_access, get_shutdown_method
from ..rpc.event.msg_type import MsgType
from ..rpc.util.wire import (
    decode,
    write_reply_into,
    reply_wire_size,
    PONG_BYTES,
    SHUTDOWN_ACK_BYTES,
)
from ..rpc.ipc.ipc_protocol import (
    IPCConfig,
    FRAME_STRUCT,
    FLAG_RESPONSE,
    FLAG_CTRL,
    encode_frame,
    encode_inline_reply_frame,
)
from ..rpc.ipc.ipc_v3_protocol import (
    FLAG_BUDDY,
    BUDDY_PAYLOAD_STRUCT,
    decode_buddy_payload,
    encode_buddy_handshake,
    decode_buddy_handshake,
    encode_buddy_reply_frame,
)
from .protocol import (
    FLAG_CALL_V2,
    FLAG_REPLY_V2,
    HANDSHAKE_V5,
    CAP_CALL_V2,
    CAP_METHOD_IDX,
    STATUS_SUCCESS,
    STATUS_ERROR,
    RouteInfo,
    MethodEntry,
    encode_v5_server_handshake,
    decode_v5_handshake,
)
from .wire import (
    MethodTable,
    decode_call_control,
    encode_v2_buddy_reply_frame,
    encode_v2_inline_reply_frame,
    encode_v2_error_reply_frame,
    _U16,
)
from .scheduler import Scheduler, ConcurrencyConfig

logger = logging.getLogger(__name__)

_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')
_SHM_NAME_RE = re.compile(r'^/?[A-Za-z0-9_.\-]{1,255}$')
FLAG_HANDSHAKE = 1 << 2

# Signal byte values (MsgType enum ints).
_PING = int(MsgType.PING)
_SHUTDOWN_CLIENT = int(MsgType.SHUTDOWN_CLIENT)


# ---------------------------------------------------------------------------
# Per-connection state
# ---------------------------------------------------------------------------

@dataclass
class _Connection:
    """Per-client connection state."""

    conn_id: int
    writer: asyncio.StreamWriter
    config: IPCConfig
    buddy_pool: object = None          # BuddyPoolHandle
    seg_views: list[memoryview] = field(default_factory=list)
    remote_segment_names: list[str] = field(default_factory=list)
    remote_segment_sizes: list[int] = field(default_factory=list)
    handshake_done: bool = False
    v2_mode: bool = False
    # In-flight submit_inline request counter.  Incremented on the event
    # loop thread before submitting; decremented by worker threads via
    # call_soon_threadsafe.  The _idle event is set when the count drops
    # to zero, allowing _handle_client's finally block to wait for all
    # pending replies before closing the writer.
    _inflight: int = field(default=0, repr=False)
    _idle: asyncio.Event | None = field(default=None, repr=False)

    def init_flight_tracking(self, loop: asyncio.AbstractEventLoop) -> None:
        self._idle = asyncio.Event()
        self._idle.set()  # starts idle
        self._loop = loop

    def flight_inc(self) -> None:
        """Mark a submit_inline request as in-flight (call from event loop)."""
        self._inflight += 1
        if self._idle is not None and self._idle.is_set():
            self._idle.clear()

    def flight_dec(self) -> None:
        """Mark a submit_inline request as completed (call from event loop via call_soon_threadsafe)."""
        self._inflight -= 1
        if self._inflight <= 0:
            self._inflight = 0
            if self._idle is not None:
                self._idle.set()

    async def wait_idle(self) -> None:
        """Wait until all in-flight submit_inline requests complete."""
        if self._idle is not None and not self._idle.is_set():
            await self._idle.wait()

    def cleanup(self) -> None:
        self.seg_views = []
        if self.buddy_pool is not None:
            try:
                self.buddy_pool.destroy()
            except Exception:
                pass
            self.buddy_pool = None


# ---------------------------------------------------------------------------
# Per-CRM routing slot
# ---------------------------------------------------------------------------

@dataclass
class CRMSlot:
    """Per-CRM registration in the server, keyed by routing name."""

    name: str
    icrm: object
    method_table: MethodTable
    scheduler: Scheduler
    methods: list[str]
    shutdown_method: str | None = None
    # Pre-built dispatch table: method_name → (callable, MethodAccess).
    _dispatch_table: dict[str, tuple[Any, MethodAccess]] = field(
        default_factory=dict, repr=False,
    )

    def build_dispatch_table(self) -> None:
        """Populate the dispatch table from the ICRM instance.

        Skips the ``@cc.on_shutdown`` method (not RPC-callable).
        """
        for name in self.methods:
            if name == self.shutdown_method:
                continue
            method = getattr(self.icrm, name, None)
            if method is not None:
                self._dispatch_table[name] = (method, get_method_access(method))


_SENTINEL = None  # Poison pill for dispatcher shutdown


class _FastDispatcher:
    """Lightweight CRM method dispatcher using SimpleQueue.

    Replaces ThreadPoolExecutor to avoid _WorkItem wrapping, heavy Queue
    locks, and per-submission Future creation overhead.
    """

    def __init__(self, loop: asyncio.AbstractEventLoop, num_workers: int = 2) -> None:
        self._loop = loop
        self._q: queue.SimpleQueue = queue.SimpleQueue()
        self._workers: list[threading.Thread] = []
        for i in range(num_workers):
            t = threading.Thread(
                target=self._worker, daemon=True, name=f'c2v2_fast_{i}',
            )
            t.start()
            self._workers.append(t)

    def submit(
        self,
        sched_execute,
        method,
        args_bytes,
        access,
        future: asyncio.Future,
    ) -> None:
        self._q.put((sched_execute, method, args_bytes, access, future, None, None, None, None))

    def submit_inline(
        self,
        sched_execute,
        method,
        args_bytes,
        access,
        request_id: int,
        writer,
        barrier=None,
        flight_dec=None,
    ) -> None:
        """Fire-and-forget: worker builds inline reply frame and writes directly.

        If *barrier* is an ``asyncio.Event``, the worker will call
        ``barrier.set()`` (via ``call_soon_threadsafe``) after execution
        completes, allowing the read loop to resume.  Used for ``@cc.write``
        methods to guarantee per-connection causal ordering.

        If *flight_dec* is provided, the worker will call it (via
        ``call_soon_threadsafe``) in the finally block to decrement the
        connection's in-flight counter.
        """
        self._q.put((sched_execute, method, args_bytes, access, None, request_id, writer, barrier, flight_dec))

    def shutdown(self) -> None:
        for _ in self._workers:
            self._q.put(_SENTINEL)
        for t in self._workers:
            t.join(timeout=2.0)

    def _worker(self) -> None:
        loop = self._loop
        q = self._q
        call_soon = loop.call_soon_threadsafe
        while True:
            item = q.get()
            if item is _SENTINEL:
                break
            sched_execute, method, args_bytes, access, future, request_id, writer, barrier, flight_dec = item
            try:
                result = sched_execute(method, args_bytes, access)
                if future is not None:
                    call_soon(future.set_result, result)
                else:
                    # Fire-and-forget: build reply and write directly
                    self._write_inline_reply(call_soon, writer, request_id, result)
            except BaseException as exc:
                if future is not None:
                    call_soon(future.set_exception, exc)
                else:
                    self._write_error_reply(call_soon, writer, request_id, exc)
            finally:
                if barrier is not None:
                    call_soon(barrier.set)
                if flight_dec is not None:
                    call_soon(flight_dec)

    @staticmethod
    def _write_inline_reply(call_soon, writer, request_id, result):
        """Build inline reply frame from ICRM result and write."""
        if isinstance(result, tuple):
            err_part = result[0] if result[0] else b''
            res_part = result[1] if len(result) > 1 and result[1] else b''
            if isinstance(err_part, memoryview):
                err_part = bytes(err_part)
            if isinstance(res_part, memoryview):
                res_part = bytes(res_part)
        elif result is None:
            res_part, err_part = b'', b''
        elif isinstance(result, memoryview):
            res_part, err_part = bytes(result), b''
        else:
            res_part, err_part = result, b''

        if err_part:
            frame = encode_v2_error_reply_frame(request_id, err_part)
        else:
            frame = encode_v2_inline_reply_frame(request_id, res_part)
        call_soon(writer.write, frame)

    @staticmethod
    def _write_error_reply(call_soon, writer, request_id, exc):
        """Build error reply frame from exception and write."""
        try:
            if hasattr(exc, 'serialize'):
                err_bytes = exc.serialize()
            else:
                err_bytes = str(exc).encode('utf-8')
        except Exception:
            err_bytes = b'Internal server error'
        call_soon(writer.write, encode_v2_error_reply_frame(request_id, err_bytes))


# ---------------------------------------------------------------------------
# ServerV2
# ---------------------------------------------------------------------------

class ServerV2:
    """IPC v3 server with wire v2 control-plane routing.

    Supports hosting **multiple CRM resources** under distinct routing
    names.  Each name has its own ICRM instance, method table, and
    scheduler.

    Handles both v1 (wire CRM_CALL in buddy SHM / inline) and v2
    (control-plane method index + pure payload) frames.  V2 frames carry
    the routing name in the control plane; v1 frames are routed to the
    default CRM.

    Parameters
    ----------
    bind_address:
        ``ipc-v3://<region_id>`` — the Unix socket will be created at
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
        region_id = bind_address.replace('ipc-v3://', '').replace('ipc://', '')
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

        Uses direct dict read (safe on CPython; asyncio hot path is
        single-threaded).  The _slots_lock is only needed for mutations.
        """
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
            name=f'c2-srv-v2-{self._address}',
            daemon=True,
        )
        self._loop_thread.start()
        if not self._started.wait(timeout):
            raise RuntimeError('ServerV2 failed to start within timeout')

    def _run_loop(self) -> None:
        self._loop = asyncio.new_event_loop()
        asyncio.set_event_loop(self._loop)
        try:
            self._loop.run_until_complete(self._async_main())
        finally:
            self._loop.close()

    async def _async_main(self) -> None:
        self._shutdown_event = asyncio.Event()
        self._dispatcher = _FastDispatcher(asyncio.get_running_loop())

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
        logger.debug('ServerV2 listening on %s', self._socket_path)

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

    # ------------------------------------------------------------------
    # Shutdown callback helper
    # ------------------------------------------------------------------

    @staticmethod
    def _invoke_shutdown(slot: CRMSlot) -> None:
        """Best-effort invocation of a CRM's ``@cc.on_shutdown`` method."""
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
        conn = _Connection(conn_id=conn_id, writer=writer, config=self._config)
        conn.init_flight_tracking(asyncio.get_running_loop())
        task = asyncio.current_task()
        self._client_tasks.append(task)

        # Pipelined dispatch: read next frame while previous dispatches
        # are still executing in the thread pool.
        pending: set[asyncio.Task] = set()

        try:
            max_frame = self._config.max_frame_size
            slots = self._slots
            default_name = self._default_name
            dispatcher = self._dispatcher
            dispatch_cache: dict[tuple[bytes, int], tuple] = {}
            cache_gen = self._slots_generation
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

                # Handshake & control: must complete before reading more.
                if flags & FLAG_HANDSHAKE:
                    await self._handle_handshake(conn, payload, writer)
                    continue
                if flags & FLAG_CTRL:
                    continue

                is_v2_call = bool(flags & FLAG_CALL_V2)
                is_buddy = bool(flags & FLAG_BUDDY)

                # Fast inline path for v2 non-buddy calls:
                # parse + dispatch table lookup + submit all without Task.
                if is_v2_call and not is_buddy:
                    name_len = payload[0]
                    try:
                        if name_len:
                            route_key = payload[1:1 + name_len]
                            method_idx = _U16.unpack_from(payload, 1 + name_len)[0]
                            args_start = 3 + name_len
                        else:
                            route_key = b''
                            method_idx = _U16.unpack_from(payload, 1)[0]
                            args_start = 3
                    except (struct.error, UnicodeDecodeError) as exc:
                        writer.write(encode_v2_error_reply_frame(
                            request_id,
                            f'Malformed v2 call control: {exc}'.encode('utf-8'),
                        ))
                        continue
                    args_bytes = payload[args_start:] if args_start < len(payload) else b''

                    # Invalidate dispatch cache when CRM registrations change.
                    cur_gen = self._slots_generation
                    if cur_gen != cache_gen:
                        dispatch_cache.clear()
                        cache_gen = cur_gen
                        slots = self._slots
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

                # Fallback: full handler via Task for buddy, v1, or error paths.
                if is_v2_call:
                    t = asyncio.create_task(
                        self._handle_v2_call(
                            conn, request_id, payload, is_buddy, writer,
                        ),
                    )
                else:
                    t = asyncio.create_task(
                        self._handle_v1_call(
                            conn, request_id, payload, is_buddy, writer,
                        ),
                    )
                pending.add(t)
                t.add_done_callback(pending.discard)

        except asyncio.IncompleteReadError:
            pass
        except asyncio.CancelledError:
            pass
        except Exception:
            logger.debug('Conn %d: handler error', conn.conn_id, exc_info=True)
        finally:
            # Drain in-flight tasks before tearing down the connection.
            if pending:
                await asyncio.gather(*pending, return_exceptions=True)
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

    # ------------------------------------------------------------------
    # Handshake
    # ------------------------------------------------------------------

    async def _handle_handshake(
        self,
        conn: _Connection,
        payload: bytes,
        writer: asyncio.StreamWriter,
    ) -> None:
        if len(payload) < 1:
            return
        version = payload[0]
        if version == HANDSHAKE_V5:
            await self._handle_v5_handshake(conn, payload, writer)
        else:
            await self._handle_v4_handshake(conn, payload, writer)

    async def _handle_v4_handshake(
        self,
        conn: _Connection,
        payload: bytes,
        writer: asyncio.StreamWriter,
    ) -> None:
        try:
            segments = decode_buddy_handshake(payload)
        except Exception as e:
            logger.warning('Conn %d: bad v4 handshake: %s', conn.conn_id, e)
            return
        self._open_segments(conn, segments)

        ack = encode_frame(0, FLAG_HANDSHAKE, encode_buddy_handshake([]))
        writer.write(ack)
        await writer.drain()

    async def _handle_v5_handshake(
        self,
        conn: _Connection,
        payload: bytes,
        writer: asyncio.StreamWriter,
    ) -> None:
        try:
            hs = decode_v5_handshake(payload)
        except Exception as e:
            logger.warning('Conn %d: bad v5 handshake: %s', conn.conn_id, e)
            return
        self._open_segments(conn, hs.segments)
        conn.v2_mode = True

        # Build route / method table ACK for *all* registered CRMs.
        route_infos: list[RouteInfo] = []
        with self._slots_lock:
            for slot in self._slots.values():
                route_infos.append(RouteInfo(
                    name=slot.name,
                    methods=[
                        MethodEntry(name=n, index=slot.method_table.index_of(n))
                        for n in slot.method_table.names()
                    ],
                ))

        cap_flags = 0
        if hs.capability_flags & CAP_CALL_V2:
            cap_flags |= CAP_CALL_V2
        if hs.capability_flags & CAP_METHOD_IDX:
            cap_flags |= CAP_METHOD_IDX

        ack_payload = encode_v5_server_handshake(
            segments=[],
            capability_flags=cap_flags,
            routes=route_infos,
        )
        writer.write(encode_frame(0, FLAG_HANDSHAKE, ack_payload))
        await writer.drain()

    _MAX_CLIENT_SEGMENTS = 16

    def _open_segments(
        self,
        conn: _Connection,
        segments: list[tuple[str, int]],
    ) -> None:
        """Open the client's buddy SHM segments and cache memoryviews."""
        if len(segments) > self._MAX_CLIENT_SEGMENTS:
            logger.warning('Conn %d: too many segments (%d > %d), rejecting handshake',
                           conn.conn_id, len(segments), self._MAX_CLIENT_SEGMENTS)
            return
        for name, _ in segments:
            if not _SHM_NAME_RE.match(name):
                logger.warning('Conn %d: invalid segment name: %r', conn.conn_id, name)
                return
        try:
            from c_two.buddy import BuddyPoolHandle, PoolConfig

            conn.buddy_pool = BuddyPoolHandle(PoolConfig(
                segment_size=self._config.pool_segment_size,
                min_block_size=4096,
                max_segments=len(segments),
                max_dedicated_segments=4,
            ))
            for name, size in segments:
                conn.buddy_pool.open_segment(name, size)
                conn.remote_segment_names.append(name)
                conn.remote_segment_sizes.append(size)

            conn.seg_views = []
            for seg_idx in range(conn.buddy_pool.segment_count()):
                base_addr, data_size = conn.buddy_pool.seg_data_info(seg_idx)
                mv = memoryview(
                    (ctypes.c_char * data_size).from_address(base_addr)
                ).cast('B')
                conn.seg_views.append(mv)

            conn.handshake_done = True
        except Exception as e:
            logger.error('Conn %d: segment open failed: %s', conn.conn_id, e)
            conn.cleanup()

    # ------------------------------------------------------------------
    # V1 call handling (legacy wire CRM_CALL format)
    # ------------------------------------------------------------------

    async def _handle_v1_call(
        self,
        conn: _Connection,
        request_id: int,
        payload: bytes,
        is_buddy: bool,
        writer: asyncio.StreamWriter,
    ) -> None:
        if is_buddy:
            method_name, args_bytes = self._resolve_v1_buddy(conn, payload)
            if method_name is None:
                return
        else:
            sig = self._check_signal(payload)
            if sig is not None:
                writer.write(encode_frame(request_id, FLAG_RESPONSE, sig))
                await writer.drain()
                if sig is SHUTDOWN_ACK_BYTES and self._shutdown_event:
                    self._shutdown_event.set()
                return

            try:
                env = decode(payload)
            except (ValueError, struct.error) as exc:
                logger.warning('Conn %d: malformed v1 wire payload: %s',
                               conn.conn_id, exc)
                return
            if env.msg_type == MsgType.PING:
                writer.write(encode_frame(request_id, FLAG_RESPONSE, PONG_BYTES))
                await writer.drain()
                return
            if env.msg_type == MsgType.SHUTDOWN_CLIENT:
                writer.write(encode_frame(request_id, FLAG_RESPONSE, SHUTDOWN_ACK_BYTES))
                await writer.drain()
                if self._shutdown_event:
                    self._shutdown_event.set()
                return
            if env.msg_type != MsgType.CRM_CALL:
                return

            method_name = env.method_name
            args_bytes = bytes(env.payload) if env.payload is not None else b''

        slot = self._resolve_slot('')
        if slot is None:
            return
        result_bytes, err_bytes = await self._dispatch(slot, method_name, args_bytes)
        await self._send_v1_reply(conn, request_id, result_bytes, err_bytes, writer)

    def _resolve_v1_buddy(
        self, conn: _Connection, payload: bytes,
    ) -> tuple[str | None, bytes]:
        """Extract method_name + args from a v1 buddy frame, free request block."""
        try:
            seg_idx, data_offset, data_size, is_dedicated, free_offset, free_size = (
                decode_buddy_payload(payload)
            )
        except (struct.error, Exception) as exc:
            logger.warning('Conn %d: malformed v1 buddy payload: %s',
                           conn.conn_id, exc)
            return None, b''
        if conn.buddy_pool is None or seg_idx >= len(conn.seg_views):
            return None, b''

        seg_mv = conn.seg_views[seg_idx]
        if data_offset + data_size > len(seg_mv):
            logger.warning('Conn %d: v1 buddy OOB: offset=%d + size=%d > seg_len=%d',
                           conn.conn_id, data_offset, data_size, len(seg_mv))
            return None, b''
        wire_mv = seg_mv[data_offset : data_offset + data_size]

        try:
            env = decode(wire_mv)
        except (ValueError, struct.error) as exc:
            logger.warning('Conn %d: malformed v1 wire data: %s',
                           conn.conn_id, exc)
            try:
                conn.buddy_pool.free_at(seg_idx, free_offset, free_size, is_dedicated)
            except Exception:
                pass
            return None, b''
        method_name = env.method_name
        args_bytes = bytes(env.payload) if env.payload is not None else b''

        try:
            conn.buddy_pool.free_at(seg_idx, free_offset, free_size, is_dedicated)
        except Exception:
            logger.warning('Conn %d: failed to free request block', conn.conn_id, exc_info=True)

        return method_name, args_bytes

    @staticmethod
    def _check_signal(payload: bytes) -> bytes | None:
        """Fast-path check for 1-byte signal messages."""
        if len(payload) >= 1:
            b = payload[0]
            if b == _PING:
                return PONG_BYTES
            if b == _SHUTDOWN_CLIENT:
                return SHUTDOWN_ACK_BYTES
        return None

    # ------------------------------------------------------------------
    # V2 call handling (control-plane routing)
    # ------------------------------------------------------------------

    async def _handle_v2_call(
        self,
        conn: _Connection,
        request_id: int,
        payload: bytes,
        is_buddy: bool,
        writer: asyncio.StreamWriter,
    ) -> None:
        if is_buddy:
            route_name, method_idx, args_bytes = self._resolve_v2_buddy(conn, payload)
            if args_bytes is None:
                return
        else:
            if not payload:
                return
            # Inline decode_call_control for speed (avoids function call +
            # tuple allocation on the hot path).
            name_len = payload[0]
            try:
                if name_len:
                    route_name = payload[1:1 + name_len].decode('utf-8')
                    method_idx = _U16.unpack_from(payload, 1 + name_len)[0]
                    args_start = 3 + name_len
                else:
                    route_name = ''
                    method_idx = _U16.unpack_from(payload, 1)[0]
                    args_start = 3
            except (struct.error, UnicodeDecodeError) as exc:
                logger.warning('Conn %d: malformed v2 call control: %s',
                               conn.conn_id, exc)
                writer.write(encode_v2_error_reply_frame(
                    request_id, f'Malformed call control: {exc}'.encode('utf-8'),
                ))
                await writer.drain()
                return
            args_bytes = payload[args_start:] if args_start < len(payload) else b''

        # Inline slot + method resolve.
        slots = self._slots
        slot = (slots.get(route_name) if route_name
                else slots.get(self._default_name or ''))
        if slot is None:
            logger.warning('Unknown route name %r', route_name)
            writer.write(encode_v2_error_reply_frame(
                request_id, f'Unknown route name: {route_name}'.encode('utf-8'),
            ))
            await writer.drain()
            return

        method_name = slot.method_table._idx_to_name.get(method_idx)
        if method_name is None:
            logger.warning('Unknown method index %d for route %r', method_idx, route_name)
            writer.write(encode_v2_error_reply_frame(
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
            writer.write(encode_v2_error_reply_frame(request_id, err_bytes))
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
            err_bytes = self._wrap_error(exc)[1]
            writer.write(encode_v2_error_reply_frame(request_id, err_bytes))
            await writer.drain()
            return

        # Inline unpack + send.
        if isinstance(result, tuple):
            err_part = result[0] if result[0] else b''
            res_part = result[1] if len(result) > 1 and result[1] else b''
            if isinstance(err_part, memoryview):
                err_part = bytes(err_part)
            if isinstance(res_part, memoryview):
                res_part = bytes(res_part)
        elif result is None:
            res_part, err_part = b'', b''
        elif isinstance(result, memoryview):
            res_part, err_part = bytes(result), b''
        else:
            res_part, err_part = result, b''

        await self._send_v2_reply(conn, request_id, res_part, err_part, writer)

    def _resolve_v2_buddy(
        self, conn: _Connection, payload: bytes,
    ) -> tuple[str, int, bytes | None]:
        """Extract route_name + method_idx + args from a v2 buddy frame."""
        try:
            seg_idx, data_offset, data_size, is_dedicated, free_offset, free_size = (
                decode_buddy_payload(payload)
            )
        except (struct.error, Exception) as exc:
            logger.warning('Conn %d: malformed v2 buddy payload: %s',
                           conn.conn_id, exc)
            return '', -1, None
        if conn.buddy_pool is None or seg_idx >= len(conn.seg_views):
            return '', -1, None

        ctrl_offset = BUDDY_PAYLOAD_STRUCT.size
        try:
            route_name, method_idx, _consumed = decode_call_control(payload, ctrl_offset)
        except (ValueError, struct.error) as exc:
            logger.warning('Conn %d: malformed v2 buddy call control: %s',
                           conn.conn_id, exc)
            return '', -1, None

        seg_mv = conn.seg_views[seg_idx]
        if data_offset + data_size > len(seg_mv):
            logger.warning('Conn %d: v2 buddy OOB: offset=%d + size=%d > seg_len=%d',
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
            return self._wrap_error(exc)

        return self._unpack_icrm_result(result)

    @staticmethod
    def _unpack_icrm_result(result: Any) -> tuple[bytes, bytes]:
        """Unpack ICRM '<-' result into ``(result_bytes, error_bytes)``."""
        if isinstance(result, tuple):
            err_part = result[0] if result[0] else b''
            res_part = result[1] if len(result) > 1 and result[1] else b''
            if isinstance(err_part, memoryview):
                err_part = bytes(err_part)
            if isinstance(res_part, memoryview):
                res_part = bytes(res_part)
            return res_part, err_part
        if result is None:
            return b'', b''
        if isinstance(result, memoryview):
            return bytes(result), b''
        return result, b''

    @staticmethod
    def _wrap_error(exc: Exception) -> tuple[bytes, bytes]:
        if isinstance(exc, error.CCBaseError):
            try:
                return b'', exc.serialize()
            except Exception:
                pass
        try:
            return b'', error.CRMExecuteFunction(str(exc)).serialize()
        except Exception:
            return b'', str(exc).encode('utf-8')

    # ------------------------------------------------------------------
    # Reply — v1 (wire CRM_REPLY in SHM or inline)
    # ------------------------------------------------------------------

    async def _send_v1_reply(
        self,
        conn: _Connection,
        request_id: int,
        result_bytes: bytes,
        err_bytes: bytes,
        writer: asyncio.StreamWriter,
    ) -> None:
        total_wire = reply_wire_size(len(err_bytes), len(result_bytes))

        if total_wire > self._config.shm_threshold and conn.buddy_pool is not None:
            try:
                alloc = conn.buddy_pool.alloc(total_wire)
                if alloc.is_dedicated:
                    conn.buddy_pool.free_at(
                        alloc.seg_idx, alloc.offset, total_wire, True,
                    )
                    raise MemoryError('dedicated segment, fall back to inline')

                seg_mv = conn.seg_views[alloc.seg_idx]
                if alloc.offset + total_wire > len(seg_mv):
                    conn.buddy_pool.free_at(
                        alloc.seg_idx, alloc.offset, total_wire, alloc.is_dedicated,
                    )
                    raise MemoryError('reply offset OOB, fall back to inline')
                write_reply_into(
                    seg_mv, alloc.offset,
                    err_bytes or None,
                    result_bytes or None,
                )
                frame = encode_buddy_reply_frame(
                    request_id, alloc.seg_idx, alloc.offset,
                    total_wire, alloc.is_dedicated,
                )
                writer.write(frame)
                if total_wire > 65536:
                    await writer.drain()
                return
            except Exception:
                pass  # Fall through to inline.

        frame = encode_inline_reply_frame(
            request_id, FLAG_RESPONSE, err_bytes, result_bytes,
        )
        writer.write(frame)
        if len(frame) > 65536:
            await writer.drain()

    # ------------------------------------------------------------------
    # Reply — v2 (control-plane status + pure payload)
    # ------------------------------------------------------------------

    async def _send_v2_reply(
        self,
        conn: _Connection,
        request_id: int,
        result_bytes: bytes,
        err_bytes: bytes,
        writer: asyncio.StreamWriter,
    ) -> None:
        if err_bytes:
            frame = encode_v2_error_reply_frame(request_id, err_bytes)
            writer.write(frame)
            await writer.drain()
            return

        data_size = len(result_bytes)

        if data_size > self._config.shm_threshold and conn.buddy_pool is not None:
            try:
                alloc = conn.buddy_pool.alloc(data_size)
                if alloc.is_dedicated:
                    conn.buddy_pool.free_at(
                        alloc.seg_idx, alloc.offset, data_size, True,
                    )
                    raise MemoryError('dedicated segment, fall back to inline')

                seg_mv = conn.seg_views[alloc.seg_idx]
                if alloc.offset + data_size > len(seg_mv):
                    conn.buddy_pool.free_at(
                        alloc.seg_idx, alloc.offset, data_size, alloc.is_dedicated,
                    )
                    raise MemoryError('reply offset OOB, fall back to inline')
                seg_mv[alloc.offset : alloc.offset + data_size] = result_bytes
                frame = encode_v2_buddy_reply_frame(
                    request_id, alloc.seg_idx, alloc.offset,
                    data_size, alloc.is_dedicated,
                )
                writer.write(frame)
                if data_size > 65536:
                    await writer.drain()
                return
            except Exception:
                pass  # Fall through to inline.

        frame = encode_v2_inline_reply_frame(request_id, result_bytes)
        writer.write(frame)
        if data_size > 65536:
            await writer.drain()
