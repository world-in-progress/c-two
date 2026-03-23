"""
IPC v2 Server — UDS control plane with SharedMemory data plane.

Control plane: asyncio Unix Domain Socket (Phase 1-2, macOS/Linux)
Data plane: multiprocessing.shared_memory.SharedMemory for large payloads

Wire protocol (control plane):
    [4B total_len][8B request_id_u64][4B flags][payload_or_shm_ref]

    total_len = 12 + payload_len  (fixed 16-byte header)

    flags (uint32, little-endian):
        bit 0: per-request SHM (legacy fallback)
        bit 1: response frame
        bit 2: pool handshake
        bit 3: pool SHM segment
"""

import asyncio
import ctypes
import logging
import os
import re
import struct
import tempfile
import hashlib
import threading
import time
from dataclasses import dataclass
from multiprocessing import shared_memory

from ... import error
from ..base import BaseServer
from ..event import Event, EventQueue, EventTag
from ..event.envelope import Envelope
from ..event.msg_type import MsgType
from ..util.adaptive_buffer import AdaptiveBuffer
from ..util.wire import encode_reply, encode_signal, decode, write_reply_into, reply_wire_size

logger = logging.getLogger(__name__)

_FLAG_SHM = 1 << 0
_FLAG_RESPONSE = 1 << 1
_FLAG_HANDSHAKE = 1 << 2
_FLAG_POOL = 1 << 3
_FAST_READ_THRESHOLD = 1_048_576            # 1 MB — use native memcpy above this size

SHM_GC_INTERVAL = 30.0                      # seconds
SHM_MAX_AGE = 120.0                         # seconds before GC considers a segment leaked

# B1: SHM name format — cc + direction char + _ + 16 hex chars (from _shm_name())
#     or pool format — ccp + direction char + _ + 12 hex chars (from pool_shm_name())
_SHM_NAME_RE = re.compile(r'^cc[a-z]_[0-9a-f]{16}$')
_POOL_SHM_NAME_RE = re.compile(r'^ccp[a-z]_[0-9a-f]{12}$')

DEFAULT_SHM_THRESHOLD = 4_096               # 4 KB — pool wins above this (benchmark 2026-03)
DEFAULT_MAX_FRAME_SIZE = 16_777_216         # 16 MB — inline frame upper bound
DEFAULT_MAX_PAYLOAD_SIZE = 17_179_869_184  # 16 GB — SHM payload upper bound (not constrained by uint32 frame)
DEFAULT_MAX_PENDING_REQUESTS = 1024         # per-server total
DEFAULT_POOL_SEGMENT_SIZE = 268_435_456     # 256 MB — per-connection pool SHM segment


@dataclass
class IPCConfig:
    shm_threshold: int = DEFAULT_SHM_THRESHOLD
    max_frame_size: int = DEFAULT_MAX_FRAME_SIZE
    max_payload_size: int = DEFAULT_MAX_PAYLOAD_SIZE
    max_pending_requests: int = DEFAULT_MAX_PENDING_REQUESTS
    pool_segment_size: int = DEFAULT_POOL_SEGMENT_SIZE
    pool_enabled: bool = True
    pool_decay_seconds: float = 60.0  # idle time before pool SHM teardown (0 = no decay)


def _shm_name(region_id: str, request_id: str, direction: str) -> str:
    # macOS limits POSIX SHM names to 31 chars (excluding leading /)
    raw = f'{region_id}_{request_id}_{direction}'.encode()
    h = hashlib.md5(raw).hexdigest()[:16]
    d = direction[0]
    return f'cc{d}_{h}'


def _encode_frame(request_id: int, flags: int, payload: bytes | bytearray | memoryview) -> bytes:
    payload_len = len(payload)
    total_len = 12 + payload_len  # 8B rid + 4B flags + payload
    if total_len > 0xFFFFFFFF:
        raise OverflowError(
            f'Frame total_len {total_len} exceeds uint32 max; '
            f'use SHM transport for payloads > {0xFFFFFFFF - 12} bytes'
        )
    buf = bytearray(4 + total_len)
    struct.pack_into('<I', buf, 0, total_len)
    struct.pack_into('<Q', buf, 4, request_id)
    struct.pack_into('<I', buf, 12, flags)
    buf[16:16 + payload_len] = payload
    return bytes(buf)


def _decode_frame(body: bytes) -> tuple[int, int, bytes]:
    body_len = len(body)
    if body_len < 12:
        raise error.EventDeserializeError(f'Frame body too small: {body_len} < 12')
    request_id = struct.unpack_from('<Q', body, 0)[0]
    flags = struct.unpack_from('<I', body, 8)[0]
    payload = body[12:]
    return request_id, flags, payload


def _fast_read_shm(
    name: str,
    size: int,
    adaptive_buf: 'AdaptiveBuffer | None' = None,
) -> tuple[memoryview, 'AdaptiveBuffer']:
    """Read SHM using native memcpy into an :class:`AdaptiveBuffer`.

    For large payloads, uses ctypes.memmove instead of
    Python's ``bytes(shm.buf)`` to avoid per-call mmap overhead.

    The adaptive buffer grows when needed and shrinks when consecutive reads
    are significantly smaller than capacity (see :mod:`~c_two.rpc.util.adaptive_buffer`).

    Returns ``(data_view, adaptive_buf)`` — pass *adaptive_buf* back on the
    next call for reuse.
    """
    if adaptive_buf is None:
        adaptive_buf = AdaptiveBuffer()

    buf = adaptive_buf.acquire(size)

    shm = shared_memory.SharedMemory(name=name, create=False, track=False)
    try:
        # Validate SHM actual size covers the declared size
        if shm.size < size:
            raise error.EventDeserializeError(
                f'SHM segment {name!r} actual size {shm.size} < declared size {size}'
            )

        if size >= _FAST_READ_THRESHOLD:
            ctypes.memmove(
                ctypes.addressof(ctypes.c_char.from_buffer(buf)),
                ctypes.addressof(ctypes.c_char.from_buffer(shm.buf)),
                size,
            )
        else:
            buf[:size] = shm.buf[:size]
    finally:
        shm.close()
        shm.unlink()
    return memoryview(buf)[:size], adaptive_buf


def _read_from_pool_shm(
    shm_buf: memoryview,
    size: int,
    adaptive_buf: 'AdaptiveBuffer | None' = None,
) -> tuple[memoryview, 'AdaptiveBuffer']:
    """Read from a pre-opened pool SHM buffer into an :class:`AdaptiveBuffer`.

    Unlike :func:`_fast_read_shm`, does **not** open or unlink the SHM — the
    handle is assumed to be pre-opened and cached from the pool handshake.
    """
    if adaptive_buf is None:
        adaptive_buf = AdaptiveBuffer()

    buf = adaptive_buf.acquire(size)

    if size >= _FAST_READ_THRESHOLD:
        ctypes.memmove(
            ctypes.addressof(ctypes.c_char.from_buffer(buf)),
            ctypes.addressof(ctypes.c_char.from_buffer(shm_buf)),
            size,
        )
    else:
        buf[:size] = shm_buf[:size]

    return memoryview(buf)[:size], adaptive_buf


async def _read_frame(reader: asyncio.StreamReader, max_frame_size: int = DEFAULT_MAX_FRAME_SIZE) -> tuple[int, int, bytes]:
    """Read a frame, returning (request_id, flags, payload) directly."""
    header = await reader.readexactly(16)  # 4B total_len + 8B rid + 4B flags
    total_len, request_id, flags = struct.unpack('<IQI', header)

    # S2: reject oversized or undersized frames before allocation
    if total_len < 12:
        raise error.EventDeserializeError(
            f'Frame too small: total_len={total_len} (minimum 12)'
        )
    if total_len > max_frame_size:
        raise error.EventDeserializeError(
            f'Frame too large: total_len={total_len} exceeds max_frame_size={max_frame_size}'
        )

    payload_len = total_len - 12
    payload = await reader.readexactly(payload_len) if payload_len > 0 else b''
    return request_id, flags, payload


async def _write_frame(writer: asyncio.StreamWriter, frame: bytes) -> None:
    writer.write(frame)
    await writer.drain()


class IPCv2Server(BaseServer):

    def __init__(self, bind_address: str, event_queue: EventQueue | None = None, ipc_config: IPCConfig | None = None):
        super().__init__(bind_address, event_queue)

        self._config = ipc_config or IPCConfig()
        self.region_id = bind_address.replace('ipc-v2://', '')
        self._socket_path = self._resolve_socket_path()

        self._loop: asyncio.AbstractEventLoop | None = None
        self._loop_thread: threading.Thread | None = None
        self._server: asyncio.AbstractServer | None = None
        self._started = threading.Event()
        self._shutdown_event = asyncio.Event()

        # Map request_id → asyncio.Future (set by reply(), awaited by handler)
        self._pending: dict[str, asyncio.Future] = {}
        self._pending_lock = threading.Lock()
        self._next_conn_id: int = 0

        # Track SHM segments we created (for response direction) that haven't been picked up
        self._our_shm_segments: dict[str, float] = {}
        self._shm_lock = threading.Lock()

        # Per-connection pool SHM (conn_id → SharedMemory)
        # Unified: client-created SHM used for both request reads and response writes.
        # Written by reply() (scheduler thread), read by _handle_client (asyncio thread)
        self._conn_pool_shm: dict[int, shared_memory.SharedMemory] = {}
        self._conn_shm_lock = threading.Lock()

        # Track active client handler tasks for clean shutdown
        self._client_tasks: set[asyncio.Task] = set()

        # Track active per-connection adaptive buffers for periodic decay
        self._conn_buffers: set[AdaptiveBuffer] = set()
        self._conn_buffers_lock = threading.Lock()

    def _resolve_socket_path(self) -> str:
        tmpdir = os.getenv('IPC_V2_SOCKET_DIR', tempfile.gettempdir())
        return os.path.join(tmpdir, f'cc_ipcv2_{self.region_id}.sock')

    # ------------------------------------------------------------------
    # BaseServer interface
    # ------------------------------------------------------------------

    def start(self) -> None:
        self._loop_thread = threading.Thread(target=self._run_loop, daemon=True)
        self._loop_thread.start()
        self._started.wait(timeout=5.0)
        if not self._started.is_set():
            raise RuntimeError('IPCv2Server failed to start within 5 seconds.')

    _event_tag_to_signal = {
        EventTag.PONG: MsgType.PONG,
        EventTag.SHUTDOWN_ACK: MsgType.SHUTDOWN_ACK,
    }

    def reply(self, event: Event) -> None:
        if not event.request_id:
            logger.warning('IPCv2Server.reply: event missing request_id')
            return

        request_id = event.request_id
        # request_id format: "conn_id:wire_rid" — extract wire rid for frame encoding
        int_rid = int(request_id.rsplit(':', 1)[1])
        flags = _FLAG_RESPONSE
        shm_name: str | None = None

        # Signal replies (PONG, SHUTDOWN_ACK) → 1-byte wire signal
        signal_type = self._event_tag_to_signal.get(event.tag)
        if signal_type is not None:
            payload = encode_signal(signal_type)
            frame = _encode_frame(int_rid, flags, payload)
            with self._pending_lock:
                fut = self._pending.pop(request_id, None)
            if fut is not None and self._loop is not None:
                self._loop.call_soon_threadsafe(fut.set_result, (frame, None))
            return

        # CRM_REPLY: extract error and result from scheduler Event
        from ..util.encoding import parse_message
        has_parts = event.data_parts is not None and event.data is None
        if has_parts:
            err_bytes = event.data_parts[0] if event.data_parts[0] else b''
            result_bytes = event.data_parts[1] if len(event.data_parts) > 1 else b''
        else:
            data = event.data if event.data is not None else b''
            parts = parse_message(data)
            err_bytes = parts[0] if len(parts) > 0 else b''
            result_bytes = parts[1] if len(parts) > 1 else b''

        err_len = len(err_bytes)
        result_len = len(result_bytes)
        total_wire = reply_wire_size(err_len, result_len)

        # Try pool SHM first (pre-allocated, zero syscalls)
        conn_id = int(request_id.rsplit(':', 1)[0])
        with self._conn_shm_lock:
            pool_shm = self._conn_pool_shm.get(conn_id)
            if pool_shm is not None and total_wire <= pool_shm.size:
                write_reply_into(pool_shm.buf, 0, err_bytes, result_bytes)
                payload = struct.pack('<Q', total_wire)
                flags |= _FLAG_POOL

        if not (flags & _FLAG_POOL):
            if total_wire >= self._config.shm_threshold:
                shm_name = _shm_name(self.region_id, request_id, 'resp')
                shm = shared_memory.SharedMemory(name=shm_name, create=True, size=total_wire)
                write_reply_into(shm.buf, 0, err_bytes, result_bytes)
                shm.close()
                size_header = struct.pack('<Q', total_wire)
                payload = shm_name.encode('utf-8') + b'\x00' + size_header
                flags |= _FLAG_SHM
            else:
                payload = encode_reply(err_bytes, result_bytes)

        frame = _encode_frame(int_rid, flags, payload)

        with self._pending_lock:
            fut = self._pending.pop(request_id, None)

        if fut is not None and self._loop is not None:
            self._loop.call_soon_threadsafe(fut.set_result, (frame, shm_name))
        else:
            logger.warning(f'IPCv2Server.reply: no pending future for request_id={request_id}')

    def shutdown(self) -> None:
        if self._loop is not None:
            self._loop.call_soon_threadsafe(self._shutdown_event.set)

    def destroy(self) -> None:
        if self._loop_thread is not None:
            self._loop_thread.join(timeout=3.0)
        try:
            os.unlink(self._socket_path)
        except FileNotFoundError:
            pass
        # Clean up any leftover per-request response SHM segments
        with self._shm_lock:
            for name in list(self._our_shm_segments):
                try:
                    shm = shared_memory.SharedMemory(name=name, create=False, track=False)
                    shm.close()
                    shm.unlink()
                except FileNotFoundError:
                    pass
            self._our_shm_segments.clear()

        # Close pool SHM handles (client owns and unlinks these)
        from .shm_pool import close_pool_shm
        with self._conn_shm_lock:
            for pool_shm in self._conn_pool_shm.values():
                close_pool_shm(pool_shm)
            self._conn_pool_shm.clear()

        with self._pending_lock:
            self._pending.clear()

    def cancel_all_calls(self) -> None:
        if self._loop is None:
            return
        with self._pending_lock:
            for fut in self._pending.values():
                self._loop.call_soon_threadsafe(fut.cancel)
            self._pending.clear()

    # ------------------------------------------------------------------
    # asyncio event loop
    # ------------------------------------------------------------------

    def _run_loop(self) -> None:
        self._loop = asyncio.new_event_loop()
        asyncio.set_event_loop(self._loop)
        try:
            self._loop.run_until_complete(self._async_run())
        finally:
            self._loop.run_until_complete(self._loop.shutdown_asyncgens())
            self._loop.close()
            self._loop = None

    async def _async_run(self) -> None:
        # Clean up stale socket
        try:
            os.unlink(self._socket_path)
        except FileNotFoundError:
            pass

        self._server = await asyncio.start_unix_server(
            self._handle_client,
            path=self._socket_path,
        )
        self._started.set()

        gc_task = asyncio.create_task(self._shm_gc_loop())

        await self._shutdown_event.wait()

        gc_task.cancel()
        try:
            await gc_task
        except asyncio.CancelledError:
            pass

        # Cancel all active client handler tasks
        for task in list(self._client_tasks):
            task.cancel()
        if self._client_tasks:
            await asyncio.gather(*self._client_tasks, return_exceptions=True)
        self._client_tasks.clear()

        self._server.close()
        await self._server.wait_closed()

        # Notify the _serve loop that the server has shut down
        if self.event_queue is not None:
            self.event_queue.put(Envelope(msg_type=MsgType.SHUTDOWN_SERVER))

    async def _handle_client(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        from .shm_pool import close_pool_shm, decode_handshake, encode_handshake

        task = asyncio.current_task()
        self._client_tasks.add(task)
        _read_buf: AdaptiveBuffer = AdaptiveBuffer()  # per-connection adaptive buffer
        with self._conn_buffers_lock:
            self._conn_buffers.add(_read_buf)
        # Assign a unique connection ID so per-connection rid counters don't collide
        conn_id = self._next_conn_id
        self._next_conn_id += 1
        conn_request_ids: set[str] = set()  # S5: track per-connection request IDs

        # Pool state for this connection (set during handshake)
        pool_shm: shared_memory.SharedMemory | None = None  # unified bidirectional
        pool_segment_size: int = 0

        # P2: event-driven shutdown — no polling timeout
        shutdown_waiter = asyncio.ensure_future(self._shutdown_event.wait())
        try:
            while True:
                read_task = asyncio.ensure_future(
                    _read_frame(reader, self._config.max_frame_size)
                )
                done, _ = await asyncio.wait(
                    {read_task, shutdown_waiter},
                    return_when=asyncio.FIRST_COMPLETED,
                )
                if shutdown_waiter in done:
                    read_task.cancel()
                    try:
                        await read_task
                    except (asyncio.CancelledError, Exception):
                        pass
                    break
                try:
                    request_id, flags, payload = read_task.result()
                except (asyncio.IncompleteReadError, ConnectionResetError):
                    break

                # ---- Pool handshake (unified bidirectional) ----
                if flags & _FLAG_HANDSHAKE:
                    try:
                        client_shm_name, seg_size = decode_handshake(payload)

                        # Validate client-provided SHM name and segment size
                        if not _POOL_SHM_NAME_RE.match(client_shm_name):
                            raise ValueError(f'Invalid pool SHM name: {client_shm_name!r}')
                        if seg_size == 0:
                            raise ValueError('Pool segment size must be > 0')

                        seg_size = min(seg_size, self._config.pool_segment_size)

                        # Clean up old pool SHM if this is a re-handshake
                        if pool_shm is not None:
                            with self._conn_shm_lock:
                                self._conn_pool_shm.pop(conn_id, None)
                            close_pool_shm(pool_shm)

                        # Open client's SHM for both reading requests and writing responses
                        pool_shm = shared_memory.SharedMemory(
                            name=client_shm_name, create=False, track=False,
                        )
                        pool_segment_size = seg_size

                        # Register for reply() access from scheduler thread
                        with self._conn_shm_lock:
                            self._conn_pool_shm[conn_id] = pool_shm

                        # Send ACK with empty name → signals unified mode
                        hs_payload = encode_handshake('', seg_size)
                        hs_frame = _encode_frame(
                            0, _FLAG_HANDSHAKE | _FLAG_RESPONSE, hs_payload,
                        )
                        await _write_frame(writer, hs_frame)
                    except Exception as exc:
                        logger.warning(
                            'IPCv2Server: pool handshake failed for conn %d: %s',
                            conn_id, exc,
                        )
                        close_pool_shm(pool_shm)
                        pool_shm = None
                        pool_segment_size = 0
                    continue

                str_rid = f'{conn_id}:{request_id}'

                # ---- Decode request data ----
                if flags & _FLAG_POOL:
                    # Pool path: read from unified pool SHM
                    if pool_shm is None or len(payload) < 8:
                        raise error.EventDeserializeError(
                            'Pool SHM frame received but no pool handshake was done'
                        )
                    size = struct.unpack('<Q', payload[:8])[0]
                    if size > pool_segment_size:
                        raise error.EventDeserializeError(
                            f'Pool payload size {size} exceeds segment size {pool_segment_size}'
                        )
                    event_bytes, _read_buf = _read_from_pool_shm(
                        pool_shm.buf, size, _read_buf,
                    )
                elif flags & _FLAG_SHM:
                    parts = payload.split(b'\x00', 1)
                    if len(parts) != 2 or len(parts[1]) < 8:
                        raise error.EventDeserializeError('Malformed SHM reference in request frame')
                    shm_name = parts[0].decode('utf-8')
                    if not _SHM_NAME_RE.match(shm_name):
                        raise error.EventDeserializeError(f'Invalid SHM name format: {shm_name!r}')
                    size = struct.unpack('<Q', parts[1])[0]
                    # S4: validate SHM payload size against config limit
                    if size > self._config.max_payload_size:
                        raise error.EventDeserializeError(
                            f'SHM payload size {size} exceeds limit {self._config.max_payload_size}'
                        )
                    event_bytes, _read_buf = _fast_read_shm(shm_name, size, _read_buf)
                else:
                    event_bytes = payload

                envelope = decode(event_bytes)
                envelope.request_id = str_rid

                # S3: enforce max pending requests
                with self._pending_lock:
                    if len(self._pending) >= self._config.max_pending_requests:
                        logger.warning(
                            f'IPCv2Server: max pending requests ({self._config.max_pending_requests}) reached, '
                            f'rejecting request_id={request_id}'
                        )
                        err_payload = encode_reply(
                            error.CCError.serialize(
                                error.CRMServerError('Server overloaded: max pending requests exceeded')
                            ),
                            b'',
                        )
                        err_frame = _encode_frame(request_id, _FLAG_RESPONSE, err_payload)
                        await _write_frame(writer, err_frame)
                        continue

                    if str_rid in self._pending:
                        logger.warning(f'IPCv2Server: duplicate request_id={request_id}, rejecting')
                        err_payload = encode_reply(
                            error.CCError.serialize(error.CRMServerError('Duplicate request ID')),
                            b'',
                        )
                        err_frame = _encode_frame(request_id, _FLAG_RESPONSE, err_payload)
                        await _write_frame(writer, err_frame)
                        continue

                    # Register a future for this request so reply() can resolve it
                    fut: asyncio.Future = self._loop.create_future()
                    self._pending[str_rid] = fut
                    conn_request_ids.add(str_rid)

                # Push into the event queue for the server's _serve loop
                self.event_queue.put(envelope)

                # Wait for reply() to provide the response frame and optional SHM name
                try:
                    response_frame, resp_shm_name = await fut
                except asyncio.CancelledError:
                    break
                finally:
                    conn_request_ids.discard(str_rid)

                await _write_frame(writer, response_frame)

                # S6: register SHM GC timestamp *after* frame is sent to client
                # (only for per-request fallback SHMs, not pool segments)
                if resp_shm_name is not None:
                    with self._shm_lock:
                        self._our_shm_segments[resp_shm_name] = time.monotonic()
        except asyncio.CancelledError:
            pass
        except Exception as exc:
            logger.debug(f'IPCv2Server client handler error: {exc}')
        finally:
            # P2: clean up shutdown waiter if still pending
            if not shutdown_waiter.done():
                shutdown_waiter.cancel()
                try:
                    await shutdown_waiter
                except asyncio.CancelledError:
                    pass
            # S5: clean up all pending futures for this connection
            with self._pending_lock:
                for rid in conn_request_ids:
                    fut = self._pending.pop(rid, None)
                    if fut is not None and not fut.done():
                        fut.cancel()
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass
            with self._conn_buffers_lock:
                self._conn_buffers.discard(_read_buf)
            _read_buf.release()
            self._client_tasks.discard(task)
            # Clean up pool SHM for this connection (close handle, client unlinks)
            close_pool_shm(pool_shm)
            with self._conn_shm_lock:
                self._conn_pool_shm.pop(conn_id, None)

    async def _shm_gc_loop(self) -> None:
        while True:
            await asyncio.sleep(SHM_GC_INTERVAL)
            now = time.monotonic()
            with self._shm_lock:
                stale = [n for n, ts in self._our_shm_segments.items() if now - ts > SHM_MAX_AGE]
                for name in stale:
                    try:
                        shm = shared_memory.SharedMemory(name=name, create=False, track=False)
                        shm.close()
                        shm.unlink()
                    except FileNotFoundError:
                        pass
                    self._our_shm_segments.pop(name, None)
                    logger.debug(f'SHM GC: cleaned up stale segment {name}')

            # Decay idle connection buffers
            with self._conn_buffers_lock:
                for abuf in self._conn_buffers:
                    abuf.maybe_decay()

    @property
    def socket_path(self) -> str:
        return self._socket_path
