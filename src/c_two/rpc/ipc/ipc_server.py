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
import logging
import os
import struct
import tempfile
import threading
import time
import uuid
from dataclasses import dataclass
from multiprocessing import shared_memory
from pathlib import Path

from ... import error
from ..base import BaseServer
from ..event import Event, EventQueue, EventTag

logger = logging.getLogger(__name__)

_FLAG_SHM = 1 << 0
_FLAG_RESPONSE = 1 << 1

DEFAULT_INLINE_THRESHOLD = 1_048_576   # 1 MB
DEFAULT_SHM_THRESHOLD = 8_388_608      # 8 MB
SHM_GC_INTERVAL = 30.0                 # seconds
SHM_MAX_AGE = 120.0                    # seconds before GC considers a segment leaked


@dataclass
class IPCConfig:
    inline_threshold: int = DEFAULT_INLINE_THRESHOLD
    shm_threshold: int = DEFAULT_SHM_THRESHOLD


def _shm_name(region_id: str, request_id: str, direction: str) -> str:
    ts = int(time.monotonic() * 1000) & 0xFFFFFFFF
    return f'cc_{region_id}_{request_id}_{direction}_{ts}'


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
    request_id = data[offset:offset + rid_len].decode('utf-8')
    offset += rid_len
    flags = struct.unpack_from('<I', data, offset)[0]
    offset += 4
    payload = data[offset:offset + (total_len - 4 - rid_len - 4)]
    return request_id, flags, payload


def _write_shm(name: str, data: bytes) -> shared_memory.SharedMemory:
    shm = shared_memory.SharedMemory(name=name, create=True, size=len(data))
    shm.buf[:len(data)] = data
    return shm


def _read_and_release_shm(name: str, size: int) -> bytes:
    shm = shared_memory.SharedMemory(name=name, create=False)
    try:
        result = bytes(shm.buf[:size])
    finally:
        shm.close()
        shm.unlink()
    return result


async def _read_frame(reader: asyncio.StreamReader) -> bytes | None:
    header = await reader.readexactly(4)
    total_len = struct.unpack('<I', header)[0]
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

        response_bytes = event.serialize()
        request_id = event.request_id
        flags = _FLAG_RESPONSE
        payload = response_bytes

        if len(response_bytes) >= self._config.shm_threshold:
            shm_name = _shm_name(self.region_id, request_id, 'resp')
            shm = _write_shm(shm_name, response_bytes)
            shm.close()  # close our handle; client takes ownership
            size_header = struct.pack('<Q', len(response_bytes))
            payload = (shm_name.encode('utf-8') + b'\x00' + size_header)
            flags |= _FLAG_SHM

            with self._shm_lock:
                self._our_shm_segments[shm_name] = time.monotonic()

        frame = _encode_frame(request_id, flags, payload)

        with self._pending_lock:
            fut = self._pending.pop(request_id, None)

        if fut is not None and self._loop is not None:
            self._loop.call_soon_threadsafe(fut.set_result, frame)
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
        # Clean up any leftover SHM segments we created
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
        try:
            while not self._shutdown_event.is_set():
                try:
                    raw = await asyncio.wait_for(_read_frame(reader), timeout=0.5)
                except asyncio.TimeoutError:
                    continue
                except (asyncio.IncompleteReadError, ConnectionResetError):
                    break

                request_id, flags, payload = _decode_frame(raw)

                # Decode inline vs SHM request data
                if flags & _FLAG_SHM:
                    parts = payload.split(b'\x00', 1)
                    shm_name = parts[0].decode('utf-8')
                    size = struct.unpack('<Q', parts[1])[0]
                    event_bytes = _read_and_release_shm(shm_name, size)
                else:
                    event_bytes = payload

                event = Event.deserialize(event_bytes)
                event.request_id = request_id

                # Register a future for this request so reply() can resolve it
                fut: asyncio.Future = self._loop.create_future()
                with self._pending_lock:
                    self._pending[request_id] = fut

                # Push into the event queue for the server's _serve loop
                self.event_queue.put(event)

                # Wait for reply() to provide the response frame
                try:
                    response_frame = await fut
                except asyncio.CancelledError:
                    break

                await _write_frame(writer, response_frame)
        except asyncio.CancelledError:
            pass
        except Exception as exc:
            logger.debug(f'IPCv2Server client handler error: {exc}')
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass
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

    @property
    def socket_path(self) -> str:
        return self._socket_path
