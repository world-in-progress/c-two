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
import re
import struct
import threading
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .. import error
from ..crm.meta import MethodAccess, get_method_access
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
    buddy_pool: object = None          # c2_buddy.BuddyPoolHandle
    seg_views: list[memoryview] = field(default_factory=list)
    remote_segment_names: list[str] = field(default_factory=list)
    remote_segment_sizes: list[int] = field(default_factory=list)
    handshake_done: bool = False
    v2_mode: bool = False

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
        method_table = MethodTable.from_methods(methods)
        cc_config = concurrency or self._default_concurrency
        scheduler = Scheduler(cc_config)

        slot = CRMSlot(
            name=routing_name,
            icrm=icrm,
            method_table=method_table,
            scheduler=scheduler,
            methods=methods,
        )
        with self._slots_lock:
            if routing_name in self._slots:
                scheduler.shutdown()
                raise ValueError(f'Name already registered: {routing_name!r}')
            self._slots[routing_name] = slot
            if self._default_name is None:
                self._default_name = routing_name
        return routing_name

    def unregister_crm(self, name: str) -> None:
        """Unregister a CRM by its routing name."""
        with self._slots_lock:
            slot = self._slots.pop(name, None)
            if slot is None:
                raise KeyError(f'Name not registered: {name!r}')
            if self._default_name == name:
                self._default_name = next(iter(self._slots), None)
        slot.scheduler.shutdown()

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
        """Look up a CRM slot by routing name (empty → default)."""
        with self._slots_lock:
            if name:
                return self._slots.get(name)
            if self._default_name is not None:
                return self._slots.get(self._default_name)
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

        os.makedirs(os.path.dirname(self._socket_path), exist_ok=True)
        if os.path.exists(self._socket_path):
            os.unlink(self._socket_path)

        self._server = await asyncio.start_unix_server(
            self._handle_client,
            path=self._socket_path,
        )
        self._started.set()
        logger.debug('ServerV2 listening on %s', self._socket_path)

        await self._shutdown_event.wait()

        # Graceful shutdown.
        for task in list(self._client_tasks):
            task.cancel()
        if self._client_tasks:
            await asyncio.gather(*self._client_tasks, return_exceptions=True)
        self._server.close()
        await self._server.wait_closed()
        with self._slots_lock:
            slots = list(self._slots.values())
        for slot in slots:
            slot.scheduler.shutdown()

    def shutdown(self) -> None:
        """Signal the server to shut down and wait for the loop thread."""
        if self._loop is not None and self._shutdown_event is not None:
            self._loop.call_soon_threadsafe(self._shutdown_event.set)
        if self._loop_thread is not None:
            self._loop_thread.join(timeout=5.0)
        try:
            os.unlink(self._socket_path)
        except OSError:
            pass

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
        task = asyncio.current_task()
        self._client_tasks.append(task)

        try:
            max_frame = self._config.max_frame_size
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

                # Dispatch.
                if flags & FLAG_HANDSHAKE:
                    await self._handle_handshake(conn, payload, writer)
                    continue
                if flags & FLAG_CTRL:
                    continue

                is_v2_call = bool(flags & FLAG_CALL_V2)
                is_buddy = bool(flags & FLAG_BUDDY)

                if is_v2_call:
                    await self._handle_v2_call(
                        conn, request_id, payload, is_buddy, writer,
                    )
                else:
                    await self._handle_v1_call(
                        conn, request_id, payload, is_buddy, writer,
                    )

        except asyncio.IncompleteReadError:
            pass
        except asyncio.CancelledError:
            pass
        except Exception:
            logger.debug('Conn %d: handler error', conn.conn_id, exc_info=True)
        finally:
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

    def _open_segments(
        self,
        conn: _Connection,
        segments: list[tuple[str, int]],
    ) -> None:
        """Open the client's buddy SHM segments and cache memoryviews."""
        for name, _ in segments:
            if not _SHM_NAME_RE.match(name):
                logger.warning('Conn %d: invalid segment name: %r', conn.conn_id, name)
                return
        try:
            import c2_buddy

            conn.buddy_pool = c2_buddy.BuddyPoolHandle(c2_buddy.PoolConfig(
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

            env = decode(payload)
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
        seg_idx, data_offset, data_size, is_dedicated, free_offset, free_size = (
            decode_buddy_payload(payload)
        )
        if conn.buddy_pool is None or seg_idx >= len(conn.seg_views):
            return None, b''

        seg_mv = conn.seg_views[seg_idx]
        wire_mv = seg_mv[data_offset : data_offset + data_size]

        env = decode(wire_mv)
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
            route_name, method_idx, _consumed = decode_call_control(payload, 0)
            args_bytes = bytes(payload[_consumed:]) if _consumed < len(payload) else b''

        slot = self._resolve_slot(route_name)
        if slot is None:
            logger.warning('Unknown route name %r', route_name)
            err_frame = encode_v2_error_reply_frame(
                request_id, f'Unknown route name: {route_name}'.encode('utf-8'),
            )
            writer.write(err_frame)
            await writer.drain()
            return

        try:
            method_name = slot.method_table.name_of(method_idx)
        except KeyError:
            logger.warning('Unknown method index %d for route %r', method_idx, route_name)
            return

        result_bytes, err_bytes = await self._dispatch(slot, method_name, args_bytes)
        await self._send_v2_reply(conn, request_id, result_bytes, err_bytes, writer)

    def _resolve_v2_buddy(
        self, conn: _Connection, payload: bytes,
    ) -> tuple[str, int, bytes | None]:
        """Extract route_name + method_idx + args from a v2 buddy frame."""
        seg_idx, data_offset, data_size, is_dedicated, free_offset, free_size = (
            decode_buddy_payload(payload)
        )
        if conn.buddy_pool is None or seg_idx >= len(conn.seg_views):
            return '', -1, None

        ctrl_offset = BUDDY_PAYLOAD_STRUCT.size
        route_name, method_idx, _consumed = decode_call_control(payload, ctrl_offset)

        seg_mv = conn.seg_views[seg_idx]
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
        method = getattr(slot.icrm, method_name, None)
        if method is None:
            err = error.CRMServerError(f'Method not found: {method_name}')
            try:
                return b'', err.serialize()
            except Exception:
                return b'', str(err).encode('utf-8')

        access = get_method_access(method)
        slot.scheduler.begin()
        loop = asyncio.get_running_loop()
        try:
            result = await loop.run_in_executor(
                slot.scheduler.executor,
                slot.scheduler.execute,
                method, args_bytes, access,
            )
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
