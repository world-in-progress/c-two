"""IPC v3 server — async UDS control plane with buddy-allocated SHM data plane.

Request-response: the server read loop awaits each request's future before
reading the next frame, giving serial request-response semantics per
connection. Both client and server share a single buddy pool and allocate
blocks for their data (requests/responses). The buddy allocator's SHM-based
spinlock provides cross-process synchronization.

Ownership model (consumer frees):
- Request blocks: client allocs, server reads via read_at, server frees via free_at
- Response blocks: server allocs, client reads via read_at, client frees via free_at
"""

from __future__ import annotations

import asyncio
import ctypes
import logging
import os
import re
import struct
import threading
import time
from pathlib import Path

from ... import error
from ..event.event import Event, EventTag
from ..event.event_queue import EventQueue
from ..event.envelope import Envelope
from ..event.msg_type import MsgType
from ..base.base_server import BaseServer
from ..util.wire import (
    decode,
    write_reply_into,
    reply_wire_size,
    payload_total_size,
    REPLY_HEADER_FIXED,
    PING_BYTES,
    PONG_BYTES,
    SHUTDOWN_CLIENT_BYTES,
    SHUTDOWN_ACK_BYTES,
)
from .ipc_protocol import (
    IPCConfig,
    FRAME_STRUCT,
    FRAME_HEADER_SIZE,
    FLAG_RESPONSE,
    FLAG_CTRL,
    U32_STRUCT,
    U64_STRUCT,
    encode_frame,
    decode_frame,
    encode_inline_reply_frame,
)
from .ipc_v3_protocol import (
    FLAG_BUDDY,
    BUDDY_PAYLOAD_SIZE,
    decode_buddy_payload,
    encode_buddy_handshake,
    decode_buddy_handshake,
    encode_buddy_reply_frame,
    encode_buddy_reuse_reply_frame,
)

logger = logging.getLogger(__name__)

_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')

# Segment name validation: alphanumeric, underscore, dash, dot; optional leading slash.
_SHM_NAME_RE = re.compile(r'^/?[A-Za-z0-9_.\-]{1,255}$')

# Pre-built mapping for signal-type replies (avoids dict creation per call).
_SIGNAL_TAGS = {
    EventTag.PONG: PONG_BYTES,
    EventTag.SHUTDOWN_ACK: SHUTDOWN_ACK_BYTES,
}


def _resolve_socket_path(region_id: str) -> str:
    sock_dir = Path(_IPC_SOCK_DIR)
    sock_dir.mkdir(parents=True, exist_ok=True)
    return str(sock_dir / f'{region_id}.sock')


class IPCv3Server(BaseServer):
    """Async Unix domain socket server with buddy-allocated SHM data plane."""

    def __init__(
        self,
        bind_address: str,
        event_queue: EventQueue | None = None,
        ipc_config: IPCConfig | None = None,
    ):
        super().__init__(bind_address, event_queue)
        self._config = ipc_config or IPCConfig()
        self.region_id = bind_address.replace('ipc-v3://', '')
        self._socket_path = _resolve_socket_path(self.region_id)

        self._loop: asyncio.AbstractEventLoop | None = None
        self._loop_thread: threading.Thread | None = None
        self._server: asyncio.AbstractServer | None = None
        self._started = threading.Event()
        self._shutdown_event: asyncio.Event | None = None

        # Per-connection state: conn_id → BuddyConnection
        self._connections: dict[int, BuddyConnection] = {}
        self._conn_lock = threading.Lock()
        self._next_conn_id = 0

        # Pending replies: str_rid → asyncio.Future[bytes] (frame bytes)
        self._pending: dict[str, asyncio.Future] = {}
        self._pending_lock = threading.Lock()

        # Deferred buddy block frees: str_rid → (pool, seg_idx, offset, size, is_ded)
        # Request blocks are freed after the CRM processes the data (in reply()).
        self._deferred_frees: dict[str, tuple] = {}
        self._deferred_frees_lock = threading.Lock()

        # Active client connection tasks for graceful shutdown.
        self._client_tasks: set[asyncio.Task] = set()

    # ------------------------------------------------------------------
    # Lifecycle (BaseServer interface)
    # ------------------------------------------------------------------

    def start(self) -> None:
        self._loop_thread = threading.Thread(target=self._run_loop, daemon=True)
        self._loop_thread.start()
        self._started.wait(timeout=5.0)
        if not self._started.is_set():
            raise RuntimeError('IPCv3Server failed to start within 5 seconds.')

    def shutdown(self) -> None:
        if self._loop is not None and self._shutdown_event is not None:
            try:
                self._loop.call_soon_threadsafe(self._shutdown_event.set)
            except RuntimeError:
                pass  # Event loop already closed.

    def destroy(self) -> None:
        if self._loop_thread is not None:
            self._loop_thread.join(timeout=3.0)
        try:
            os.unlink(self._socket_path)
        except OSError:
            pass
        # Cancel all pending futures before clearing connections.
        self.cancel_all_calls()
        # Drain deferred frees
        with self._deferred_frees_lock:
            deferred = dict(self._deferred_frees)
            self._deferred_frees.clear()
        for rid_key, info in deferred.items():
            try:
                pool, seg_idx, offset, data_size, is_dedicated = info[:5]
                pool.free_at(seg_idx, offset, data_size, is_dedicated)
            except Exception:
                pass  # pool may already be destroyed
        with self._conn_lock:
            for conn in self._connections.values():
                conn.cleanup()
            self._connections.clear()
        with self._pending_lock:
            for fut in self._pending.values():
                if not fut.done():
                    fut.cancel()
            self._pending.clear()

    def cancel_all_calls(self) -> None:
        loop = self._loop
        if loop is None:
            return
        with self._pending_lock:
            futs = list(self._pending.values())
            self._pending.clear()
        for fut in futs:
            try:
                loop.call_soon_threadsafe(fut.cancel)
            except RuntimeError:
                pass  # Loop already closed.

    # ------------------------------------------------------------------
    # Deferred buddy block free
    # ------------------------------------------------------------------

    def _free_deferred(self, str_rid: str) -> None:
        """Free a deferred buddy request block (if any) for this request ID."""
        with self._deferred_frees_lock:
            info = self._deferred_frees.pop(str_rid, None)
        if info is not None:
            pool, seg_idx, offset, size, is_dedicated = info[:5]
            try:
                pool.free_at(seg_idx, offset, size, is_dedicated)
            except Exception:
                pass

    # ------------------------------------------------------------------
    # reply() — called from scheduler thread
    # ------------------------------------------------------------------

    def reply(self, event: Event) -> None:
        """Build response frame and resolve the pending future.

        Called from the scheduler thread. Thread-safe via asyncio
        call_soon_threadsafe.

        The deferred request block is freed AFTER write_reply_into so that
        memoryview data_parts (pointing into the request SHM block) remain
        valid during the SHM→SHM copy.
        """
        rid_key = event.request_id
        if not rid_key:
            return

        with self._pending_lock:
            fut = self._pending.pop(rid_key, None)
        if fut is None:
            self._free_deferred(rid_key)
            return

        # Parse composite key once: "conn_id:request_id"
        sep = rid_key.rfind(':')
        conn_id = int(rid_key[:sep])
        int_rid = int(rid_key[sep + 1:])

        # Extract error and result bytes from the scheduler Event.
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

        # Signal-type replies (PONG, SHUTDOWN_ACK).
        signal_payload = _SIGNAL_TAGS.get(event.tag)
        if signal_payload is not None:
            self._free_deferred(rid_key)
            frame = encode_frame(int_rid, FLAG_RESPONSE, signal_payload)
            loop = self._loop
            if loop is not None:
                try:
                    loop.call_soon_threadsafe(self._resolve_future, fut, frame)
                except RuntimeError:
                    pass
            return

        err_len = len(err_bytes)
        result_len = payload_total_size(result_bytes)
        total_wire = reply_wire_size(err_len, result_len)

        # Snapshot connection's buddy pool + cached views under lock, then release.
        with self._conn_lock:
            conn = self._connections.get(conn_id)
            buddy_pool = conn.buddy_pool if conn is not None and conn.handshake_done else None
            seg_views = conn.seg_views if conn is not None else []

        # ---- Zero-copy reuse path ----
        # When the CRM returns the exact input memoryview (echo pattern), skip
        # allocation and copy entirely: overwrite the CRM_CALL header in-place
        # with a CRM_REPLY header and point the response to the request block.
        if (isinstance(result_bytes, memoryview) and err_len == 0
                and buddy_pool is not None
                and total_wire > self._config.shm_threshold):
            # Atomically pop to avoid TOCTOU race: another thread could pop
            # the same entry between a get() and a later pop().
            with self._deferred_frees_lock:
                info = self._deferred_frees.pop(rid_key, None)
            if info is not None and len(info) >= 8 and info[5] is not None:
                (_pool, req_seg_idx, block_offset, alloc_size,
                 _ded, payload_offset, payload_len, seg_obj) = info
                if (len(result_bytes) == payload_len
                        and req_seg_idx < len(seg_views)
                        and result_bytes.obj is seg_obj):
                    reply_start = payload_offset - REPLY_HEADER_FIXED
                    if reply_start >= block_offset:
                        seg_mv = seg_views[req_seg_idx]
                        seg_mv[reply_start] = MsgType.CRM_REPLY
                        struct.pack_into('<I', seg_mv, reply_start + 1, 0)
                        reply_data_size = REPLY_HEADER_FIXED + payload_len
                        frame = encode_buddy_reuse_reply_frame(
                            int_rid, req_seg_idx, reply_start, reply_data_size,
                            block_offset, alloc_size,
                        )
                        loop = self._loop
                        if loop is not None:
                            try:
                                loop.call_soon_threadsafe(
                                    self._resolve_future, fut, frame)
                            except RuntimeError:
                                pass
                        return
                # Reuse failed — re-register deferred free so regular path
                # or _free_deferred() can release the block.
                with self._deferred_frees_lock:
                    self._deferred_frees[rid_key] = info

        # ---- Scatter-reuse path for tuple serialize results ----
        # When serialize returns (header_bytes, blob_memoryview) and the blob
        # is a memoryview into the request SHM block at the correct offset,
        # we only need to write the reply header (5 bytes) and the serialize
        # header — the blob stays in-place.  This avoids materializing and
        # copying 1GB+ of data on the response path.
        if (isinstance(result_bytes, (tuple, list)) and len(result_bytes) == 2
                and err_len == 0
                and buddy_pool is not None
                and total_wire > self._config.shm_threshold):
            hdr_part, blob_part = result_bytes
            if (isinstance(blob_part, memoryview)
                    and isinstance(hdr_part, (bytes, bytearray))):
                with self._deferred_frees_lock:
                    info = self._deferred_frees.get(rid_key)
                if info is not None and len(info) >= 8 and info[5] is not None:
                    (_pool, req_seg_idx, block_offset, alloc_size,
                     _ded, payload_offset, payload_len, seg_obj) = info
                    hdr_len = len(hdr_part)
                    blob_len = len(blob_part)
                    if (blob_part.obj is seg_obj
                            and req_seg_idx < len(seg_views)
                            and hdr_len + blob_len == payload_len):
                        # Verify blob is at the expected position: the output
                        # header size must match the input header size so the
                        # blob doesn't need to shift.
                        seg_mv = seg_views[req_seg_idx]
                        seg_addr = ctypes.addressof(
                            (ctypes.c_char * 1).from_buffer(seg_mv))
                        blob_addr = ctypes.addressof(
                            (ctypes.c_char * 1).from_buffer(blob_part))
                        blob_seg_offset = blob_addr - seg_addr
                        expected_blob_offset = payload_offset + hdr_len
                        if blob_seg_offset == expected_blob_offset:
                            reply_start = payload_offset - REPLY_HEADER_FIXED
                            if reply_start >= block_offset:
                                # Write reply header (5 bytes).
                                seg_mv[reply_start] = MsgType.CRM_REPLY
                                struct.pack_into('<I', seg_mv,
                                                 reply_start + 1, 0)
                                # Write serialize header (overwrites the
                                # original — may differ if CRM transformed
                                # metadata fields).
                                seg_mv[payload_offset:
                                       payload_offset + hdr_len] = hdr_part
                                # Blob stays in place — zero copy!
                                reply_data_size = (REPLY_HEADER_FIXED
                                                   + hdr_len + blob_len)
                                frame = encode_buddy_reuse_reply_frame(
                                    int_rid, req_seg_idx, reply_start,
                                    reply_data_size, block_offset, alloc_size,
                                )
                                loop = self._loop
                                if loop is not None:
                                    try:
                                        loop.call_soon_threadsafe(
                                            self._resolve_future, fut, frame)
                                    except RuntimeError:
                                        pass
                                return

        # ---- Regular buddy alloc + copy path ----
        frame: bytes | None = None
        freed_deferred = False
        if buddy_pool is not None and total_wire > self._config.shm_threshold:
            try:
                alloc = buddy_pool.alloc(total_wire)
                if alloc.is_dedicated:
                    buddy_pool.free_at(
                        alloc.seg_idx, alloc.offset, total_wire, True,
                    )
                    raise ValueError('dedicated segment, inline fallback')
                seg_mv = seg_views[alloc.seg_idx]
                shm_buf = seg_mv[alloc.offset : alloc.offset + total_wire]
                write_reply_into(shm_buf, 0, err_bytes, result_bytes)
                frame = encode_buddy_reply_frame(
                    int_rid, alloc.seg_idx, alloc.offset,
                    total_wire, alloc.is_dedicated,
                )
            except Exception:
                # Alloc failed or dedicated — materialize any references into
                # the request SHM block before freeing it, then retry.
                if isinstance(result_bytes, memoryview):
                    result_bytes = bytes(result_bytes)
                elif isinstance(result_bytes, (tuple, list)):
                    # Scatter-write tuples may contain memoryview segments
                    # pointing into the request SHM block — flatten to bytes
                    # to avoid aliasing corruption when the retry alloc reuses
                    # the same block.
                    result_bytes = b''.join(
                        bytes(s) if isinstance(s, memoryview) else s
                        for s in result_bytes
                    )
                result_len = payload_total_size(result_bytes)
                total_wire = reply_wire_size(err_len, result_len)
                self._free_deferred(rid_key)
                freed_deferred = True
                try:
                    alloc = buddy_pool.alloc(total_wire)
                    if alloc.is_dedicated:
                        buddy_pool.free_at(
                            alloc.seg_idx, alloc.offset, total_wire, True,
                        )
                        raise ValueError('dedicated segment, inline fallback')
                    seg_mv = seg_views[alloc.seg_idx]
                    shm_buf = seg_mv[alloc.offset : alloc.offset + total_wire]
                    write_reply_into(shm_buf, 0, err_bytes, result_bytes)
                    frame = encode_buddy_reply_frame(
                        int_rid, alloc.seg_idx, alloc.offset,
                        total_wire, alloc.is_dedicated,
                    )
                except Exception as e:
                    logger.warning('Buddy alloc for reply failed, inline fallback: %s', e)
                    frame = None

        # Free the request block AFTER writing the response — result_bytes
        # may be a memoryview into the request SHM block (zero-copy path).
        if not freed_deferred:
            self._free_deferred(rid_key)

        if frame is None:
            # Flatten tuple payloads for inline frame encoder.
            inline_result = b''.join(result_bytes) if isinstance(result_bytes, (list, tuple)) else result_bytes
            frame = encode_inline_reply_frame(int_rid, FLAG_RESPONSE, err_bytes, inline_result)

        loop = self._loop
        if loop is not None:
            try:
                loop.call_soon_threadsafe(self._resolve_future, fut, frame)
            except RuntimeError:
                pass

    @staticmethod
    def _resolve_future(fut: asyncio.Future, frame: bytes) -> None:
        if not fut.done():
            fut.set_result(frame)

    # ------------------------------------------------------------------
    # Async event loop
    # ------------------------------------------------------------------

    def _run_loop(self) -> None:
        self._loop = asyncio.new_event_loop()
        asyncio.set_event_loop(self._loop)
        try:
            self._loop.run_until_complete(self._async_main())
        finally:
            self._loop.close()

    async def _async_main(self) -> None:
        self._shutdown_event = asyncio.Event()

        try:
            os.unlink(self._socket_path)
        except OSError:
            pass

        self._server = await asyncio.start_unix_server(
            self._handle_client,
            path=self._socket_path,
        )
        self._started.set()
        logger.debug('IPCv3Server listening on %s', self._socket_path)

        await self._shutdown_event.wait()

        # Graceful shutdown: cancel client tasks, close server.
        tasks_snapshot = set(self._client_tasks)
        for task in tasks_snapshot:
            task.cancel()
        if tasks_snapshot:
            await asyncio.gather(*tasks_snapshot, return_exceptions=True)
        self._client_tasks.clear()
        self._server.close()
        await self._server.wait_closed()

        # Notify the _serve loop that the server has shut down.
        if self.event_queue is not None:
            self.event_queue.put(Envelope(msg_type=MsgType.SHUTDOWN_SERVER))

    # ------------------------------------------------------------------
    # Client connection handler
    # ------------------------------------------------------------------

    async def _handle_client(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
    ) -> None:
        task = asyncio.current_task()
        self._client_tasks.add(task)

        with self._conn_lock:
            conn_id = self._next_conn_id
            self._next_conn_id += 1
            conn = BuddyConnection(conn_id, writer, self._config)
            self._connections[conn_id] = conn

        try:
            max_frame = self._config.max_frame_size
            while True:
                # Inline frame read (avoids coroutine creation overhead).
                header = await reader.readexactly(16)
                total_len, request_id, flags = FRAME_STRUCT.unpack(header)
                if total_len < 12:
                    raise error.EventDeserializeError(f'Frame too small: {total_len}')
                if total_len > max_frame:
                    raise error.EventDeserializeError(f'Frame too large: {total_len}')
                payload_len = total_len - 12
                payload = await reader.readexactly(payload_len) if payload_len > 0 else b''

                # Dispatch by flag.
                if flags & (1 << 2):  # FLAG_HANDSHAKE
                    await self._handle_buddy_handshake(conn, payload, writer)
                    continue
                if flags & FLAG_CTRL:
                    continue

                # Build composite request ID early so _resolve_request can
                # register deferred frees keyed by it.
                str_rid = f'{conn_id}:{request_id}'

                # Resolve wire bytes from frame.
                wire_bytes = self._resolve_request(conn, flags, payload, str_rid)
                if wire_bytes is None:
                    continue

                # Parse wire message into Envelope.
                envelope = decode(wire_bytes)

                # Handle signals (PING, SHUTDOWN).
                if envelope.msg_type == MsgType.PING:
                    self._free_deferred(str_rid)
                    pong_frame = encode_frame(request_id, FLAG_RESPONSE, PONG_BYTES)
                    writer.write(pong_frame)
                    await writer.drain()
                    continue
                if envelope.msg_type in (MsgType.SHUTDOWN_CLIENT, MsgType.SHUTDOWN_SERVER):
                    self._free_deferred(str_rid)
                    ack_frame = encode_frame(request_id, FLAG_RESPONSE, SHUTDOWN_ACK_BYTES)
                    writer.write(ack_frame)
                    await writer.drain()
                    self._shutdown_event.set()
                    break

                # CRM_CALL: register future, enqueue, await reply.
                with self._pending_lock:
                    if len(self._pending) >= self._config.max_pending_requests:
                        logger.warning(
                            'Conn %d: pending requests limit reached (%d)',
                            conn_id, self._config.max_pending_requests,
                        )
                        self._free_deferred(str_rid)
                        continue

                envelope.request_id = str_rid

                fut = self._loop.create_future()
                with self._pending_lock:
                    self._pending[str_rid] = fut

                self.event_queue.put(envelope)

                try:
                    response_frame = await fut
                except asyncio.CancelledError:
                    break

                writer.write(response_frame)
                # Skip drain for small buddy frames — UDS kernel buffer handles them.
                if len(response_frame) > 65536:
                    await writer.drain()

        except (ConnectionResetError, BrokenPipeError, asyncio.CancelledError,
                asyncio.IncompleteReadError, OSError):
            pass
        except Exception:
            logger.exception('Conn %d: unhandled error', conn_id)
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass
            with self._conn_lock:
                self._connections.pop(conn_id, None)
            # Cancel any pending futures for this connection.
            prefix = f'{conn_id}:'
            with self._pending_lock:
                to_cancel = [k for k in self._pending if k.startswith(prefix)]
                for k in to_cancel:
                    f = self._pending.pop(k, None)
                    if f and not f.done():
                        f.cancel()
            # Free any deferred buddy blocks for this connection.
            with self._deferred_frees_lock:
                deferred_keys = [k for k in self._deferred_frees if k.startswith(prefix)]
                for k in deferred_keys:
                    info = self._deferred_frees.pop(k, None)
                    if info:
                        pool, si, off, sz, ded = info[:5]
                        try:
                            pool.free_at(si, off, sz, ded)
                        except Exception:
                            pass
            conn.cleanup()
            self._client_tasks.discard(task)

    def _resolve_request(
        self,
        conn: BuddyConnection,
        flags: int,
        payload: bytes | memoryview,
        str_rid: str | None = None,
    ) -> bytes | memoryview | None:
        """Resolve wire bytes from a data frame. Handles buddy and inline.

        For buddy frames, returns a zero-copy memoryview into SHM and defers
        the block free until reply() — this avoids a full memcpy per request.
        """
        if flags & FLAG_BUDDY:
            seg_idx, offset, data_size, is_dedicated, _, _ = decode_buddy_payload(payload)
            # Snapshot references under lock to protect against destroy() race.
            with self._conn_lock:
                if conn.buddy_pool is None:
                    logger.warning('Conn %d: buddy frame before handshake', conn.conn_id)
                    return None
                if seg_idx >= len(conn.seg_views):
                    logger.warning(
                        'Conn %d: invalid seg_idx %d (max %d)',
                        conn.conn_id, seg_idx, len(conn.seg_views) - 1,
                    )
                    return None
                seg_mv = conn.seg_views[seg_idx]
                buddy_pool = conn.buddy_pool
            # Bounds-check the read region within the segment.
            if offset + data_size > len(seg_mv):
                logger.warning(
                    'Conn %d: buddy read OOB offset=%d size=%d seg_len=%d',
                    conn.conn_id, offset, data_size, len(seg_mv),
                )
                try:
                    buddy_pool.free_at(seg_idx, offset, data_size, is_dedicated)
                except Exception:
                    pass
                return None
            # Zero-copy: return memoryview into SHM, defer free until reply().
            if str_rid is not None:
                wire_mv = seg_mv[offset : offset + data_size]
                # Peek at CRM_CALL header to record payload position for
                # zero-copy response reuse in reply().
                payload_offset = None
                payload_len = 0
                seg_obj = None
                if data_size >= 3 and wire_mv[0] == MsgType.CRM_CALL:
                    method_len = struct.unpack_from('<H', wire_mv, 1)[0]
                    hdr_end = 3 + method_len
                    if data_size > hdr_end:
                        payload_offset = offset + hdr_end
                        payload_len = data_size - hdr_end
                        seg_obj = seg_mv.obj
                with self._deferred_frees_lock:
                    self._deferred_frees[str_rid] = (
                        buddy_pool, seg_idx, offset, data_size, is_dedicated,
                        payload_offset, payload_len, seg_obj,
                    )
                return wire_mv
            # Fallback: copy if no rid (shouldn't happen in normal flow).
            wire_bytes = bytes(seg_mv[offset : offset + data_size])
            try:
                buddy_pool.free_at(seg_idx, offset, data_size, is_dedicated)
            except Exception:
                pass
            return wire_bytes
        return bytes(payload) if isinstance(payload, memoryview) else payload

    # ------------------------------------------------------------------
    # Handshake
    # ------------------------------------------------------------------

    async def _handle_buddy_handshake(
        self,
        conn: BuddyConnection,
        payload: bytes | memoryview,
        writer: asyncio.StreamWriter,
    ) -> None:
        """Process v4 buddy handshake: client sends pool segments, server opens them."""
        try:
            segments = decode_buddy_handshake(payload)
        except Exception as e:
            logger.warning('Bad buddy handshake: %s', e)
            return

        MAX_SEGMENTS = self._config.max_pool_segments
        if len(segments) > MAX_SEGMENTS:
            logger.warning(
                'Conn %d: handshake seg_count %d exceeds limit %d',
                conn.conn_id, len(segments), MAX_SEGMENTS,
            )
            return

        for name, _size in segments:
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

            # Cache persistent memoryview for each opened segment data region.
            conn.seg_views = []
            for seg_idx in range(conn.buddy_pool.segment_count()):
                base_addr, data_size = conn.buddy_pool.seg_data_info(seg_idx)
                mv = memoryview(
                    (ctypes.c_char * data_size).from_address(base_addr)
                ).cast('B')
                conn.seg_views.append(mv)

            conn.handshake_done = True
        except Exception as e:
            logger.error('Conn %d: handshake failed: %s', conn.conn_id, e)
            conn.cleanup()
            return

        ack_payload = encode_buddy_handshake([])
        ack_frame = encode_frame(0, 1 << 2, ack_payload)  # FLAG_HANDSHAKE
        writer.write(ack_frame)
        await writer.drain()


class BuddyConnection:
    """Per-connection state for the IPC v3 server."""

    __slots__ = (
        'conn_id', 'writer', 'config', 'buddy_pool',
        'remote_segment_names', 'remote_segment_sizes',
        'handshake_done', 'last_activity', 'seg_views',
    )

    def __init__(self, conn_id: int, writer: asyncio.StreamWriter, config: IPCConfig):
        self.conn_id = conn_id
        self.writer = writer
        self.config = config
        self.buddy_pool = None
        self.remote_segment_names: list[str] = []
        self.remote_segment_sizes: list[int] = []
        self.handshake_done = False
        self.last_activity = time.monotonic()
        self.seg_views: list[memoryview] = []

    def cleanup(self) -> None:
        self.seg_views = []
        if self.buddy_pool is not None:
            try:
                self.buddy_pool.destroy()
            except Exception:
                pass
            self.buddy_pool = None


async def _read_frame(
    reader: asyncio.StreamReader,
    max_frame_size: int,
) -> tuple[int, int, bytes]:
    """Read a complete frame, returning (request_id, flags, payload)."""
    header = await reader.readexactly(16)
    total_len, request_id, flags = FRAME_STRUCT.unpack(header)
    if total_len < 12:
        raise error.EventDeserializeError(f'Frame too small: {total_len}')
    if total_len > max_frame_size:
        raise error.EventDeserializeError(f'Frame too large: {total_len}')
    payload_len = total_len - 12
    payload = await reader.readexactly(payload_len) if payload_len > 0 else b''
    return request_id, flags, payload


def _recv_exact(sock, n: int) -> bytes:
    """Receive exactly n bytes from a blocking socket."""
    data = bytearray()
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise ConnectionResetError('Connection closed')
        data.extend(chunk)
    return bytes(data)
