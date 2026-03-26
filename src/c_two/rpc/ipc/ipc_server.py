"""
IPC v2 Server — UDS control plane with SharedMemory data plane.

Control plane: asyncio Unix Domain Socket (Phase 1-2, macOS/Linux)
Data plane: multiprocessing.shared_memory.SharedMemory for large payloads

Split pool architecture:
    - Client Outbound Pool: client creates, client writes requests, server reads
    - Server Response Pool: server creates, server writes responses, client reads
    - Each side manages their own pool independently

Wire protocol (control plane):
    [4B total_len][8B request_id_u64][4B flags][payload_or_shm_ref]

    total_len = 12 + payload_len  (fixed 16-byte header)

    flags (uint32, little-endian):
        bit 0: per-request SHM (legacy fallback)
        bit 1: response frame
        bit 2: pool handshake
        bit 3: pool SHM segment
        bit 4: control message (segment announce, consumed)
"""

import asyncio
import logging
import os
import re
import struct
import tempfile
import threading
import time
from dataclasses import dataclass, field
from multiprocessing import shared_memory

from ... import error
from ..base import BaseServer
from ..event import Event, EventQueue, EventTag
from ..event.envelope import Envelope
from ..event.msg_type import MsgType
from ..util.adaptive_buffer import AdaptiveBuffer
from ..util.wire import encode_reply, encode_signal, decode, write_reply_into, reply_wire_size, PING_BYTES
from .ipc_protocol import (
    FLAG_SHM, FLAG_POOL, FLAG_HANDSHAKE, FLAG_RESPONSE, FLAG_CTRL,
    FRAME_STRUCT, U64_STRUCT, U32_STRUCT,
    FAST_READ_THRESHOLD, FRAME_HEADER_SIZE, POOL_PAYLOAD_HEADER_SIZE,
    SHM_GC_INTERVAL, SHM_MAX_AGE,
    SegmentState,
    CTRL_SEGMENT_ANNOUNCE, CTRL_CONSUMED,
    POOL_DIR_OUTBOUND, POOL_DIR_RESPONSE,
    IPCConfig, DEFAULT_MAX_FRAME_SIZE,
    DEFAULT_MAX_PAYLOAD_SIZE, DEFAULT_SHM_THRESHOLD,
    DEFAULT_MAX_PENDING_REQUESTS, DEFAULT_POOL_SEGMENT_SIZE,
    encode_frame, decode_frame,
    encode_inline_reply_frame,
    encode_ctrl_segment_announce, decode_ctrl_segment_announce,
    encode_ctrl_consumed, decode_ctrl_consumed,
    shm_name, fast_read_shm, read_from_pool_shm,
)

logger = logging.getLogger(__name__)

# B1: SHM name format — cc + direction char + pid_hex(1-6) + _ + hash_hex
#     or pool format — ccp + direction char + pid_hex(1-6) + _ + hash_hex
#     or split pool — ccpo/ccps + pid_hex(1-6) + _ + hash_hex
_SHM_NAME_RE = re.compile(r'^cc[a-z][0-9a-f]{1,6}_[0-9a-f]{10,15}$')
_POOL_SHM_NAME_RE = re.compile(r'^ccp[a-z][0-9a-f]{1,6}_[0-9a-f]{9,14}$')
_SPLIT_POOL_SHM_RE = re.compile(r'^ccp[os][0-9a-f]{1,6}_[0-9a-f]{9,14}$')


def _valid_pool_shm_name(name: str) -> bool:
    """Validate a pool SHM name (legacy or split format)."""
    return bool(_POOL_SHM_NAME_RE.match(name) or _SPLIT_POOL_SHM_RE.match(name))


@dataclass
class _ClientContext:
    """Per-connection state for _handle_client.

    Split pool architecture:
        - client_pool_*: Client's outbound pool (server reads requests from these)
        - resp_pool_*: Server's response pool (server writes responses to these)
    """
    conn_id: int
    reader: asyncio.StreamReader
    writer: asyncio.StreamWriter
    read_buf: AdaptiveBuffer = field(default_factory=AdaptiveBuffer)
    conn_request_ids: set[str] = field(default_factory=set)

    # Client's outbound pool (client creates, server opens read-only)
    client_pool_shms: list[shared_memory.SharedMemory] = field(default_factory=list)
    client_pool_sizes: list[int] = field(default_factory=list)

    # Server's response pool (server creates and manages)
    resp_pool_shms: list[shared_memory.SharedMemory] = field(default_factory=list)
    resp_pool_sizes: list[int] = field(default_factory=list)
    resp_pool_names: list[str] = field(default_factory=list)
    resp_pool_budget_used: int = 0

    last_activity: float = field(default_factory=time.monotonic)
    _heartbeat_task: asyncio.Task | None = None

    def get_client_pool_shm(self, index: int) -> shared_memory.SharedMemory | None:
        """Return the client outbound pool SHM at *index*, or None."""
        if 0 <= index < len(self.client_pool_shms):
            return self.client_pool_shms[index]
        return None

    def get_resp_pool_shm(self, index: int) -> shared_memory.SharedMemory | None:
        """Return the server response pool SHM at *index*, or None."""
        if 0 <= index < len(self.resp_pool_shms):
            return self.resp_pool_shms[index]
        return None


async def _read_frame(reader: asyncio.StreamReader, max_frame_size: int = DEFAULT_MAX_FRAME_SIZE) -> tuple[int, int, bytes]:
    """Read a frame, returning (request_id, flags, payload) directly."""
    header = await reader.readexactly(16)  # 4B total_len + 8B rid + 4B flags
    total_len, request_id, flags = FRAME_STRUCT.unpack(header)

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

        # Per-connection response pool (conn_id → (shm_list, size_list))
        # Server-created SHM used for writing responses to the client.
        # Written by reply() (scheduler thread), read by _handle_client (asyncio thread)
        self._conn_resp_pool: dict[int, tuple[list[shared_memory.SharedMemory], list[int]]] = {}
        self._conn_resp_lock = threading.Lock()

        # Per-connection pending CTRL frames to send before response
        # Written by reply() (scheduler thread), read by _handle_client (asyncio thread)
        self._conn_ctrl_frames: dict[int, list[bytes]] = {}
        self._conn_ctrl_lock = threading.Lock()

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

    def _resolve_pending(self, request_id: str, frame: bytes, shm_name_val: str | None = None) -> None:
        """Pop pending future and deliver response frame to the event loop."""
        with self._pending_lock:
            fut = self._pending.pop(request_id, None)
        if fut is not None and self._loop is not None:
            self._loop.call_soon_threadsafe(fut.set_result, (frame, shm_name_val))
        else:
            logger.warning(f'IPCv2Server.reply: no pending future for request_id={request_id}')

    def reply(self, event: Event) -> None:
        if not event.request_id:
            logger.warning('IPCv2Server.reply: event missing request_id')
            return

        request_id = event.request_id
        int_rid = int(request_id.rsplit(':', 1)[1])
        flags = FLAG_RESPONSE
        resp_shm_name: str | None = None

        # Signal replies (PONG, SHUTDOWN_ACK) → 1-byte wire signal
        signal_type = self._event_tag_to_signal.get(event.tag)
        if signal_type is not None:
            payload = encode_signal(signal_type)
            frame = encode_frame(int_rid, flags, payload)
            self._resolve_pending(request_id, frame)
            return

        # CRM_REPLY: extract error and result from scheduler Event
        has_parts = event.data_parts is not None and event.data is None
        if has_parts:
            err_bytes = event.data_parts[0] if event.data_parts[0] else b''
            result_bytes = event.data_parts[1] if len(event.data_parts) > 1 else b''
        else:
            from ..util.encoding import parse_message
            data = event.data if event.data is not None else b''
            parts = parse_message(data)
            err_bytes = parts[0] if len(parts) > 0 else b''
            result_bytes = parts[1] if len(parts) > 1 else b''

        err_len = len(err_bytes)
        result_len = len(result_bytes)
        total_wire = reply_wire_size(err_len, result_len)

        # Try server's response pool first (pre-allocated, zero syscalls)
        conn_id = int(request_id.rsplit(':', 1)[0])
        with self._conn_resp_lock:
            pool_entry = self._conn_resp_pool.get(conn_id)
            if pool_entry is not None:
                resp_shms, resp_sizes = pool_entry
                # First-fit: find a segment large enough
                for idx, (pool_shm, seg_size) in enumerate(zip(resp_shms, resp_sizes)):
                    if total_wire <= seg_size:
                        write_reply_into(pool_shm.buf, 0, err_bytes, result_bytes)
                        payload = struct.pack('<BQ', idx, total_wire)
                        flags |= FLAG_POOL
                        break

                # Expansion: no segment fits, try to create a new one
                if not (flags & FLAG_POOL) and total_wire >= self._config.shm_threshold:
                    new_size = max(self._config.pool_segment_size, total_wire)
                    # Check budget and segment count
                    budget_used = sum(resp_sizes)
                    can_expand = (
                        len(resp_shms) < self._config.max_pool_segments
                        and budget_used + new_size <= self._config.max_pool_memory
                        and new_size <= self._config.max_payload_size
                    )
                    if can_expand:
                        try:
                            from .shm_pool import pool_shm_name_response, create_pool_shm
                            new_name = pool_shm_name_response(
                                self.region_id, conn_id * 1000 + len(resp_shms),
                            )
                            new_shm = create_pool_shm(new_name, new_size)
                            new_seg_size = min(new_size, new_shm.size)
                            new_index = len(resp_shms)

                            resp_shms.append(new_shm)
                            resp_sizes.append(new_seg_size)

                            # Queue CTRL_SEGMENT_ANNOUNCE for the asyncio thread to send
                            ctrl_payload = encode_ctrl_segment_announce(
                                POOL_DIR_RESPONSE, new_index, new_seg_size, new_name,
                            )
                            ctrl_frame = encode_frame(0, FLAG_CTRL, ctrl_payload)
                            with self._conn_ctrl_lock:
                                self._conn_ctrl_frames.setdefault(conn_id, []).append(ctrl_frame)

                            # Also track the name for cleanup
                            # (We need ctx for this, but ctx is in asyncio thread.
                            #  Store in a side dict for cleanup_connection to read.)
                            with self._shm_lock:
                                self._our_shm_segments[new_name] = 0.0  # 0.0 = pool, not timed

                            # Write into the new segment
                            if total_wire <= new_seg_size:
                                write_reply_into(new_shm.buf, 0, err_bytes, result_bytes)
                                payload = struct.pack('<BQ', new_index, total_wire)
                                flags |= FLAG_POOL
                        except Exception as exc:
                            logger.warning('Server response pool expansion failed: %s', exc)

        if not (flags & FLAG_POOL):
            if total_wire >= self._config.shm_threshold:
                resp_shm_name = shm_name(self.region_id, request_id, 'resp')
                shm = shared_memory.SharedMemory(name=resp_shm_name, create=True, size=total_wire)
                write_reply_into(shm.buf, 0, err_bytes, result_bytes)
                shm.close()
                size_header = U64_STRUCT.pack(total_wire)
                payload = resp_shm_name.encode('utf-8') + b'\x00' + size_header
                flags |= FLAG_SHM
            else:
                frame = encode_inline_reply_frame(int_rid, flags, err_bytes, result_bytes)
                self._resolve_pending(request_id, frame)
                return

        frame = encode_frame(int_rid, flags, payload)
        self._resolve_pending(request_id, frame, resp_shm_name)

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

        # Close client outbound pool handles (client owns and unlinks these)
        # Unlink server response pool handles (server owns these)
        from .shm_pool import close_pool_shm
        with self._conn_resp_lock:
            for resp_shms, _ in self._conn_resp_pool.values():
                for pool_shm in resp_shms:
                    close_pool_shm(pool_shm, unlink=True)
            self._conn_resp_pool.clear()

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
        """Orchestrator for a single client connection lifecycle."""
        task = asyncio.current_task()
        self._client_tasks.add(task)

        conn_id = self._next_conn_id
        self._next_conn_id += 1
        ctx = _ClientContext(conn_id=conn_id, reader=reader, writer=writer)

        with self._conn_buffers_lock:
            self._conn_buffers.add(ctx.read_buf)

        # Start heartbeat probe task if enabled
        if self._config.heartbeat_interval > 0:
            ctx._heartbeat_task = asyncio.create_task(self._heartbeat_loop(ctx))

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

                # Any received frame counts as activity
                ctx.last_activity = time.monotonic()

                if flags & FLAG_HANDSHAKE:
                    await self._handle_pool_handshake(ctx, payload)
                    continue

                if flags & FLAG_CTRL:
                    self._handle_ctrl_message(ctx, payload)
                    continue

                str_rid = f'{conn_id}:{request_id}'
                wire_bytes = self._decode_request(ctx, flags, payload)

                envelope = decode(wire_bytes)
                envelope.request_id = str_rid

                response_frame, resp_shm_name = await self._dispatch_request(
                    ctx, str_rid, request_id, envelope, writer,
                )
                if response_frame is None:
                    continue

                # Send any pending CTRL frames before the response
                # (e.g. SEGMENT_ANNOUNCE for newly expanded response pool)
                with self._conn_ctrl_lock:
                    ctrl_frames = self._conn_ctrl_frames.pop(conn_id, None)
                if ctrl_frames:
                    for cf in ctrl_frames:
                        await _write_frame(writer, cf)

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
            await self._cleanup_connection(ctx, shutdown_waiter, task)

    async def _heartbeat_loop(self, ctx: _ClientContext) -> None:
        """Periodically send PING frames and detect dead connections."""
        interval = self._config.heartbeat_interval
        timeout = self._config.heartbeat_timeout
        ping_frame = encode_frame(0, 0, PING_BYTES)
        try:
            while True:
                await asyncio.sleep(interval)
                elapsed = time.monotonic() - ctx.last_activity
                if elapsed >= timeout:
                    logger.debug(
                        'IPCv2Server: conn %d heartbeat timeout (%.1fs idle)',
                        ctx.conn_id, elapsed,
                    )
                    # Force-close the transport to unblock the read loop
                    ctx.writer.close()
                    return
                try:
                    await _write_frame(ctx.writer, ping_frame)
                except (ConnectionError, OSError):
                    ctx.writer.close()
                    return
        except asyncio.CancelledError:
            pass

    async def _handle_pool_handshake(self, ctx: _ClientContext, payload: bytes) -> None:
        """Negotiate split pool with the client (v3 handshake).

        Client sends its outbound pool info; server creates response pool
        and sends ACK with both negotiated sizes and response pool info.
        """
        from .shm_pool import (
            close_pool_shm, decode_handshake, encode_handshake_ack,
            pool_shm_name_response, create_pool_shm,
        )
        try:
            client_shm_name, seg_size, segment_index = decode_handshake(payload)

            if not _valid_pool_shm_name(client_shm_name):
                raise ValueError(f'Invalid pool SHM name: {client_shm_name!r}')
            if seg_size == 0:
                raise ValueError('Pool segment size must be > 0')

            if segment_index != 0:
                # v3 does not use append handshake — expansion uses CTRL_SEGMENT_ANNOUNCE
                raise ValueError(
                    f'v3 handshake does not support segment_index > 0 '
                    f'(got {segment_index}); use CTRL_SEGMENT_ANNOUNCE instead'
                )

            # Initial handshake — clamp client's requested size to server's default
            seg_size = min(seg_size, self._config.pool_segment_size)

            # Clean up old pools if this is a re-handshake
            if ctx.client_pool_shms:
                with self._conn_resp_lock:
                    self._conn_resp_pool.pop(ctx.conn_id, None)
                for old_shm in ctx.client_pool_shms:
                    close_pool_shm(old_shm)
                ctx.client_pool_shms.clear()
                ctx.client_pool_sizes.clear()
                for old_shm in ctx.resp_pool_shms:
                    close_pool_shm(old_shm, unlink=True)
                ctx.resp_pool_shms.clear()
                ctx.resp_pool_sizes.clear()
                ctx.resp_pool_names.clear()
                ctx.resp_pool_budget_used = 0

            # Open client's outbound pool (read-only from server's perspective)
            client_shm = shared_memory.SharedMemory(
                name=client_shm_name, create=False, track=False,
            )
            negotiated_size = min(seg_size, client_shm.size)
            ctx.client_pool_shms.append(client_shm)
            ctx.client_pool_sizes.append(negotiated_size)

            # Create server's response pool segment[0]
            resp_seg_size = self._config.pool_segment_size
            resp_name = pool_shm_name_response(self.region_id, ctx.conn_id)
            resp_shm = create_pool_shm(resp_name, resp_seg_size)
            actual_resp_size = min(resp_seg_size, resp_shm.size)
            ctx.resp_pool_shms.append(resp_shm)
            ctx.resp_pool_sizes.append(actual_resp_size)
            ctx.resp_pool_names.append(resp_name)
            ctx.resp_pool_budget_used = actual_resp_size

            # Register response pool for reply() access from scheduler thread
            with self._conn_resp_lock:
                self._conn_resp_pool[ctx.conn_id] = (ctx.resp_pool_shms, ctx.resp_pool_sizes)

            # Send v3 ACK with client negotiated size + server response pool info
            hs_payload = encode_handshake_ack(
                negotiated_size, actual_resp_size, resp_name,
            )
            hs_frame = encode_frame(
                0, FLAG_HANDSHAKE | FLAG_RESPONSE, hs_payload,
            )
            await _write_frame(ctx.writer, hs_frame)
        except Exception as exc:
            logger.warning(
                'IPCv2Server: pool handshake failed for conn %d: %s',
                ctx.conn_id, exc,
            )

    def _handle_ctrl_message(self, ctx: _ClientContext, payload: bytes) -> None:
        """Process a control message from the client."""
        if len(payload) < 1:
            logger.warning('Empty CTRL payload from conn %d', ctx.conn_id)
            return
        ctrl_type = payload[0]

        if ctrl_type == CTRL_SEGMENT_ANNOUNCE:
            direction, index, size, name = decode_ctrl_segment_announce(payload)
            if direction != POOL_DIR_OUTBOUND:
                logger.warning(
                    'Server received SEGMENT_ANNOUNCE for direction=%d (expected outbound)',
                    direction,
                )
                return
            if not _valid_pool_shm_name(name):
                logger.warning('Invalid pool SHM name in SEGMENT_ANNOUNCE: %r', name)
                return
            if index != len(ctx.client_pool_shms):
                logger.warning(
                    'SEGMENT_ANNOUNCE index mismatch: expected %d, got %d',
                    len(ctx.client_pool_shms), index,
                )
                return
            try:
                shm = shared_memory.SharedMemory(name=name, create=False, track=False)
                negotiated_size = min(size, shm.size)
                ctx.client_pool_shms.append(shm)
                ctx.client_pool_sizes.append(negotiated_size)
            except Exception as exc:
                logger.warning('Failed to open announced segment %r: %s', name, exc)

        elif ctrl_type == CTRL_CONSUMED:
            # Client consumed a response pool segment — in half-duplex this
            # is redundant (segment is already free) but we accept it for
            # protocol consistency and future full-duplex.
            pass

        else:
            logger.warning('Unknown CTRL type %d from conn %d', ctrl_type, ctx.conn_id)

    def _decode_request(self, ctx: _ClientContext, flags: int, payload: bytes) -> bytes | memoryview:
        """Resolve request wire bytes from inline, per-request SHM, or client outbound pool."""
        if flags & FLAG_POOL:
            if not ctx.client_pool_shms or len(payload) < POOL_PAYLOAD_HEADER_SIZE:
                raise error.EventDeserializeError(
                    'Pool SHM frame received but no pool handshake was done'
                )
            segment_index = payload[0]
            size = U64_STRUCT.unpack(payload[1:9])[0]
            pool_shm = ctx.get_client_pool_shm(segment_index)
            if pool_shm is None:
                raise error.EventDeserializeError(
                    f'Pool segment_index {segment_index} out of range '
                    f'(have {len(ctx.client_pool_shms)} segments)'
                )
            seg_size = ctx.client_pool_sizes[segment_index]
            if size > seg_size:
                raise error.EventDeserializeError(
                    f'Pool payload size {size} exceeds segment size {seg_size}'
                )
            # Zero-copy: decode directly from SHM memoryview
            # Safe because client is blocked waiting for response
            return pool_shm.buf[:size]
        elif flags & FLAG_SHM:
            parts = payload.split(b'\x00', 1)
            if len(parts) != 2 or len(parts[1]) < 8:
                raise error.EventDeserializeError('Malformed SHM reference in request frame')
            req_shm_name = parts[0].decode('utf-8')
            if not _SHM_NAME_RE.match(req_shm_name):
                raise error.EventDeserializeError(f'Invalid SHM name format: {req_shm_name!r}')
            size = U64_STRUCT.unpack(parts[1])[0]
            # S4: validate SHM payload size against config limit
            if size > self._config.max_payload_size:
                raise error.EventDeserializeError(
                    f'SHM payload size {size} exceeds limit {self._config.max_payload_size}'
                )
            event_bytes, ctx.read_buf = fast_read_shm(req_shm_name, size, ctx.read_buf)
            return event_bytes
        else:
            return payload

    async def _dispatch_request(
        self,
        ctx: _ClientContext,
        str_rid: str,
        wire_rid: int,
        envelope: Envelope,
        writer: asyncio.StreamWriter,
    ) -> tuple[bytes | None, str | None]:
        """Register pending future, enqueue envelope, and await reply."""
        # S3: enforce max pending requests
        with self._pending_lock:
            if len(self._pending) >= self._config.max_pending_requests:
                logger.warning(
                    f'IPCv2Server: max pending requests ({self._config.max_pending_requests}) reached, '
                    f'rejecting request_id={wire_rid}'
                )
                err_payload = encode_reply(
                    error.CCError.serialize(
                        error.CRMServerError('Server overloaded: max pending requests exceeded')
                    ),
                    b'',
                )
                err_frame = encode_frame(wire_rid, FLAG_RESPONSE, err_payload)
                await _write_frame(writer, err_frame)
                return None, None

            if str_rid in self._pending:
                logger.warning(f'IPCv2Server: duplicate request_id={wire_rid}, rejecting')
                err_payload = encode_reply(
                    error.CCError.serialize(error.CRMServerError('Duplicate request ID')),
                    b'',
                )
                err_frame = encode_frame(wire_rid, FLAG_RESPONSE, err_payload)
                await _write_frame(writer, err_frame)
                return None, None

            fut: asyncio.Future = self._loop.create_future()
            self._pending[str_rid] = fut
            ctx.conn_request_ids.add(str_rid)

        self.event_queue.put(envelope)

        try:
            response_frame, resp_shm_name = await fut
        except asyncio.CancelledError:
            raise
        finally:
            ctx.conn_request_ids.discard(str_rid)

        return response_frame, resp_shm_name

    async def _cleanup_connection(
        self,
        ctx: _ClientContext,
        shutdown_waiter: asyncio.Future,
        task: asyncio.Task,
    ) -> None:
        """Release all resources held by a client connection."""
        from .shm_pool import close_pool_shm

        # Cancel heartbeat task if running
        if ctx._heartbeat_task is not None and not ctx._heartbeat_task.done():
            ctx._heartbeat_task.cancel()
            try:
                await ctx._heartbeat_task
            except asyncio.CancelledError:
                pass

        # P2: clean up shutdown waiter if still pending
        if not shutdown_waiter.done():
            shutdown_waiter.cancel()
            try:
                await shutdown_waiter
            except asyncio.CancelledError:
                pass
        # S5: clean up all pending futures for this connection
        with self._pending_lock:
            for rid in ctx.conn_request_ids:
                fut = self._pending.pop(rid, None)
                if fut is not None and not fut.done():
                    fut.cancel()
        ctx.writer.close()
        try:
            await ctx.writer.wait_closed()
        except Exception:
            pass
        with self._conn_buffers_lock:
            self._conn_buffers.discard(ctx.read_buf)
        ctx.read_buf.release()
        self._client_tasks.discard(task)

        # Clean up client outbound pool (close handles only — client unlinks)
        for pool_shm in ctx.client_pool_shms:
            close_pool_shm(pool_shm)

        # Clean up server response pool (close + unlink — server owns these)
        with self._conn_resp_lock:
            self._conn_resp_pool.pop(ctx.conn_id, None)
        for pool_shm in ctx.resp_pool_shms:
            close_pool_shm(pool_shm, unlink=True)
        # Remove response pool names from GC tracking
        with self._shm_lock:
            for name in ctx.resp_pool_names:
                self._our_shm_segments.pop(name, None)

        # Clean up any pending CTRL frames
        with self._conn_ctrl_lock:
            self._conn_ctrl_frames.pop(ctx.conn_id, None)

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

                # NOTE: PID-based orphan detection for _our_shm_segments is
                # unnecessary — these are server-created segments whose
                # embedded PID is always this process.  PID-based scanning
                # of the filesystem (for client-created orphans) is deferred
                # to the P1 memory-pressure monitor.

            # Decay idle connection buffers
            with self._conn_buffers_lock:
                for abuf in self._conn_buffers:
                    abuf.maybe_decay()

    @property
    def socket_path(self) -> str:
        return self._socket_path
