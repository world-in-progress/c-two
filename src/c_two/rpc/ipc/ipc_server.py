"""
IPC v2 Server — UDS control plane with SharedMemory data plane.

Control plane: asyncio Unix Domain Socket (Phase 1-2, macOS/Linux)
Data plane: multiprocessing.shared_memory.SharedMemory for large payloads
Ownership transfer: sender creates SHM → receiver reads and releases

Wire protocol (control plane):
    [4B total_len][4B request_id_len][request_id][4B flags][payload_or_shm_ref]

    flags (uint32, little-endian):
        bit 0: 0 = inline, 1 = shared_memory
        bit 1: 0 = request,  1 = response
        bit 2-31: reserved
"""

import asyncio
import ctypes
import logging
import os
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
from ..util.adaptive_buffer import AdaptiveBuffer

logger = logging.getLogger(__name__)

_FLAG_SHM = 1 << 0
_FLAG_RESPONSE = 1 << 1

DEFAULT_SHM_THRESHOLD = 1_048_576      # 1 MB — aligned with inline threshold
SHM_GC_INTERVAL = 30.0                 # seconds
SHM_MAX_AGE = 120.0                    # seconds before GC considers a segment leaked


DEFAULT_MAX_FRAME_SIZE = 16_777_216     # 16 MB — inline frame upper bound
DEFAULT_MAX_PAYLOAD_SIZE = 4_294_967_296  # 4 GB — SHM payload upper bound
DEFAULT_MAX_PENDING_REQUESTS = 1024    # per-server total


@dataclass
class IPCConfig:
    shm_threshold: int = DEFAULT_SHM_THRESHOLD
    max_frame_size: int = DEFAULT_MAX_FRAME_SIZE
    max_payload_size: int = DEFAULT_MAX_PAYLOAD_SIZE
    max_pending_requests: int = DEFAULT_MAX_PENDING_REQUESTS


def _shm_name(region_id: str, request_id: str, direction: str) -> str:
    # macOS limits POSIX SHM names to 31 chars (excluding leading /)
    raw = f'{region_id}_{request_id}_{direction}'.encode()
    h = hashlib.md5(raw).hexdigest()[:16]
    d = direction[0]
    return f'cc{d}_{h}'


def _encode_frame(request_id: str, flags: int, payload: bytes) -> bytes:
    rid = request_id.encode('utf-8')
    rid_len = len(rid)
    total_len = 4 + rid_len + 4 + len(payload)
    return (
        struct.pack('<I', total_len)
        + struct.pack('<I', rid_len)
        + rid
        + struct.pack('<I', flags)
        + payload
    )


def _decode_frame(data: bytes) -> tuple[str, int, bytes]:
    offset = 0
    total_len = struct.unpack_from('<I', data, offset)[0]
    offset += 4
    rid_len = struct.unpack_from('<I', data, offset)[0]
    offset += 4

    # Validate rid_len doesn't overflow the frame
    payload_len = total_len - 4 - rid_len - 4
    if rid_len > total_len - 8 or payload_len < 0:
        raise error.EventDeserializeError(
            f'Malformed frame: rid_len={rid_len} exceeds available space (total_len={total_len})'
        )

    request_id = data[offset:offset + rid_len].decode('utf-8')
    offset += rid_len
    flags = struct.unpack_from('<I', data, offset)[0]
    offset += 4
    payload = data[offset:offset + payload_len]
    return request_id, flags, payload


# Phase 4A: native memcpy + pre-allocated buffer
_FAST_READ_THRESHOLD = 1_048_576  # 1 MB — use native memcpy above this size


def _fast_read_shm(
    name: str,
    size: int,
    adaptive_buf: 'AdaptiveBuffer | None' = None,
) -> tuple[memoryview, 'AdaptiveBuffer']:
    """Read SHM using native memcpy into an :class:`AdaptiveBuffer`.

    For large payloads, uses ctypes.memmove (~40 GB/s on M1 Max) instead of
    Python's ``bytes(shm.buf)`` (~6-25 GB/s) to avoid per-call mmap overhead.

    The adaptive buffer grows when needed and shrinks when consecutive reads
    are significantly smaller than capacity (see :mod:`~c_two.rpc.util.adaptive_buffer`).

    Returns ``(data_view, adaptive_buf)`` — pass *adaptive_buf* back on the
    next call for reuse.
    """
    if adaptive_buf is None:
        adaptive_buf = AdaptiveBuffer()

    view = adaptive_buf.acquire(size)
    buf = adaptive_buf.raw_buffer

    shm = shared_memory.SharedMemory(name=name, create=False)
    try:
        # S4: validate SHM actual size covers the declared size
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


# ---------------------------------------------------------------------------
# Phase 3 helpers — scatter-write & zero-copy read
# ---------------------------------------------------------------------------

def _scatter_write_event_to_shm(
    name: str,
    tag: 'EventTag',
    data: bytes | memoryview,
) -> tuple[shared_memory.SharedMemory, int]:
    """Write Event(tag, data) directly to SHM without intermediate serialization.

    Produces the same binary layout as serializing ``Event(tag, data=data)``
    into SHM but with only ONE copy of *data* into the SHM buffer.
    """
    tag_bytes = tag.value.encode('utf-8')
    tag_len = len(tag_bytes)
    data_len = len(data)
    total = 8 + tag_len + 8 + data_len

    shm = shared_memory.SharedMemory(name=name, create=True, size=total)
    buf = shm.buf
    offset = 0

    struct.pack_into('>Q', buf, offset, tag_len);  offset += 8
    buf[offset:offset + tag_len] = tag_bytes;       offset += tag_len
    struct.pack_into('>Q', buf, offset, data_len);  offset += 8
    if data_len > 0:
        buf[offset:offset + data_len] = data
    return shm, total


def _scatter_write_event_multi_to_shm(
    name: str,
    tag: 'EventTag',
    messages: list[bytes | memoryview],
) -> tuple[shared_memory.SharedMemory, int]:
    """Scatter-write an Event whose data is ``concat(add_length_prefix(m) for m in messages)``.

    Equivalent to building the combined prefixed messages then writing to SHM,
    but avoids **all** intermediate allocations — only one copy of each message
    into the SHM buffer.
    """
    tag_bytes = tag.value.encode('utf-8')
    tag_len = len(tag_bytes)
    data_size = sum(8 + len(m) for m in messages)
    total = 8 + tag_len + 8 + data_size

    shm = shared_memory.SharedMemory(name=name, create=True, size=total)
    buf = shm.buf
    offset = 0

    struct.pack_into('>Q', buf, offset, tag_len);   offset += 8
    buf[offset:offset + tag_len] = tag_bytes;        offset += tag_len
    struct.pack_into('>Q', buf, offset, data_size);  offset += 8

    for msg in messages:
        msg_len = len(msg)
        struct.pack_into('>Q', buf, offset, msg_len); offset += 8
        if msg_len > 0:
            buf[offset:offset + msg_len] = msg
            offset += msg_len

    return shm, total


async def _read_frame(reader: asyncio.StreamReader, max_frame_size: int = DEFAULT_MAX_FRAME_SIZE) -> bytes | None:
    header = await reader.readexactly(4)
    total_len = struct.unpack('<I', header)[0]

    # S2: reject oversized or undersized frames before allocation
    if total_len < 8:
        raise error.EventDeserializeError(
            f'Frame too small: total_len={total_len} (minimum 8)'
        )
    if total_len > max_frame_size:
        raise error.EventDeserializeError(
            f'Frame too large: total_len={total_len} exceeds max_frame_size={max_frame_size}'
        )

    body = await reader.readexactly(total_len)
    return header + body


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

        # Track SHM segments we created (for response direction) that haven't been picked up
        self._our_shm_segments: dict[str, float] = {}
        self._shm_lock = threading.Lock()

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

    def reply(self, event: Event) -> None:
        if not event.request_id:
            logger.warning('IPCv2Server.reply: event missing request_id')
            return

        request_id = event.request_id
        flags = _FLAG_RESPONSE
        shm_name: str | None = None

        # Phase 4B: scatter-write response parts directly (avoid concat)
        has_parts = event.data_parts is not None and event.data is None
        if has_parts:
            estimated_size = sum(8 + len(p) for p in event.data_parts) + 30
        else:
            data = event.data if event.data is not None else b''
            estimated_size = len(data) + 30

        if estimated_size >= self._config.shm_threshold:
            shm_name = _shm_name(self.region_id, request_id, 'resp')
            if has_parts:
                shm, written = _scatter_write_event_multi_to_shm(
                    shm_name, event.tag, event.data_parts,
                )
            else:
                shm, written = _scatter_write_event_to_shm(shm_name, event.tag, data)
            shm.close()
            size_header = struct.pack('<Q', written)
            payload = (shm_name.encode('utf-8') + b'\x00' + size_header)
            flags |= _FLAG_SHM
        else:
            payload = event.serialize()

        frame = _encode_frame(request_id, flags, payload)

        # S6: pass (frame, shm_name) via future — GC timestamp registered
        # by _handle_client after _write_frame succeeds
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
        # Clean up any leftover response SHM segments
        with self._shm_lock:
            for name in list(self._our_shm_segments):
                try:
                    shm = shared_memory.SharedMemory(name=name, create=False)
                    shm.close()
                    shm.unlink()
                except FileNotFoundError:
                    pass
            self._our_shm_segments.clear()

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

    async def _handle_client(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        task = asyncio.current_task()
        self._client_tasks.add(task)
        _read_buf: AdaptiveBuffer = AdaptiveBuffer()  # per-connection adaptive buffer
        with self._conn_buffers_lock:
            self._conn_buffers.add(_read_buf)
        conn_request_ids: set[str] = set()  # S5: track per-connection request IDs
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
                    raw = read_task.result()
                except (asyncio.IncompleteReadError, ConnectionResetError):
                    break

                request_id, flags, payload = _decode_frame(raw)

                # Decode inline vs SHM request data
                if flags & _FLAG_SHM:
                    parts = payload.split(b'\x00', 1)
                    if len(parts) != 2 or len(parts[1]) < 8:
                        raise error.EventDeserializeError('Malformed SHM reference in request frame')
                    shm_name = parts[0].decode('utf-8')
                    size = struct.unpack('<Q', parts[1])[0]
                    # S4: validate SHM payload size against config limit
                    if size > self._config.max_payload_size:
                        raise error.EventDeserializeError(
                            f'SHM payload size {size} exceeds limit {self._config.max_payload_size}'
                        )
                    event_bytes, _read_buf = _fast_read_shm(shm_name, size, _read_buf)
                else:
                    event_bytes = payload

                event = Event.deserialize(event_bytes)
                event.request_id = request_id

                # S3: enforce max pending requests
                with self._pending_lock:
                    if len(self._pending) >= self._config.max_pending_requests:
                        logger.warning(
                            f'IPCv2Server: max pending requests ({self._config.max_pending_requests}) reached, '
                            f'rejecting request_id={request_id}'
                        )
                        err_event = Event(tag=EventTag.CRM_REPLY, request_id=request_id)
                        err_event.data = error.CCError.serialize(
                            error.CRMServerError('Server overloaded: max pending requests exceeded')
                        )
                        err_frame = _encode_frame(request_id, _FLAG_RESPONSE, err_event.serialize())
                        await _write_frame(writer, err_frame)
                        continue

                    # Register a future for this request so reply() can resolve it
                    fut: asyncio.Future = self._loop.create_future()
                    self._pending[request_id] = fut
                    conn_request_ids.add(request_id)

                # Push into the event queue for the server's _serve loop
                self.event_queue.put(event)

                # Wait for reply() to provide the response frame and optional SHM name
                try:
                    response_frame, resp_shm_name = await fut
                except asyncio.CancelledError:
                    break
                finally:
                    conn_request_ids.discard(request_id)

                await _write_frame(writer, response_frame)

                # S6: register SHM GC timestamp *after* frame is sent to client
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

    async def _shm_gc_loop(self) -> None:
        while True:
            await asyncio.sleep(SHM_GC_INTERVAL)
            now = time.monotonic()
            with self._shm_lock:
                stale = [n for n, ts in self._our_shm_segments.items() if now - ts > SHM_MAX_AGE]
                for name in stale:
                    try:
                        shm = shared_memory.SharedMemory(name=name, create=False)
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
