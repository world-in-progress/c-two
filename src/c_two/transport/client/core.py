"""Concurrent multiplexed IPC client.

Unlike the serial :class:`IPCClient` (which holds ``_conn_lock`` for the
entire call duration), :class:`SharedClient` uses a background receive thread
and per-request :class:`PendingCall` objects to support concurrent calls from
multiple ICRM consumers over a single UDS connection and buddy pool.

Memory model:
- One UDS connection per SharedClient (vs one per ICRM consumer)
- One buddy pool (256 MB default) shared across all callers
- N ICRM consumers share 1 SharedClient → N × 256 MB → 256 MB

Ownership model (same as IPCClient):
- Request blocks: client allocs, server frees after reading
- Response blocks: server allocs, client frees after reading

Control-plane routing negotiated via handshake.  SHM contains pure
payload (no wire header); method routing via 2-byte index in the inline
control frame.
"""
from __future__ import annotations

import ctypes
import logging
import math
import os
import socket as _socket
import struct
import threading
from dataclasses import dataclass, field
from pathlib import Path

from ... import error
from ..ipc.msg_type import (
    MsgType,
    PING_BYTES,
    SHUTDOWN_CLIENT_BYTES,
    SHUTDOWN_ACK_BYTES,
)
from ..ipc.frame import (
    IPCConfig,
    FRAME_STRUCT,
    FLAG_RESPONSE,
    encode_frame,
)
from ..ipc.buddy import (
    FLAG_BUDDY,
    decode_buddy_payload,
)
from ..protocol import (
    FLAG_CALL,
    FLAG_REPLY,
    FLAG_CHUNKED,
    FLAG_CHUNK_LAST,
    FLAG_SIGNAL,
    HANDSHAKE_VERSION,
    CAP_CALL,
    CAP_METHOD_IDX,
    CAP_CHUNKED,
    STATUS_SUCCESS,
    STATUS_ERROR,
    Handshake,
    RouteInfo,
    encode_client_handshake,
    decode_handshake,
)
from ..wire import (
    CHUNK_HEADER_SIZE,
    MethodTable,
    payload_total_size,
    encode_buddy_call_frame,
    encode_inline_call_frame,
    encode_buddy_chunked_call_frame,
    encode_inline_chunked_call_frame,
    decode_chunk_header,
    decode_reply_control,
)

logger = logging.getLogger(__name__)

_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')


def _resolve_socket_path(region_id: str) -> str:
    return str(Path(_IPC_SOCK_DIR) / f'{region_id}.sock')


# ---------------------------------------------------------------------------
# PendingCall — per-request synchronisation primitive
# ---------------------------------------------------------------------------

@dataclass
class PendingCall:
    """Holds result state for one in-flight RPC call.

    The calling thread creates a PendingCall, registers it in
    ``SharedClient._pending``, sends the request, then calls :meth:`wait`.
    The background recv thread calls :meth:`set_result` or :meth:`set_error`
    when the matching response arrives.
    """

    rid: int
    _event: threading.Event = field(default_factory=threading.Event, repr=False)
    result: bytes | None = field(default=None, repr=False)
    error_exc: Exception | None = field(default=None, repr=False)

    def wait(self, timeout: float | None = None) -> bytes:
        if not self._event.wait(timeout):
            raise TimeoutError(f'RPC call {self.rid} timed out')
        if self.error_exc is not None:
            raise self.error_exc
        return self.result if self.result is not None else b''

    def set_result(self, data: bytes) -> None:
        self.result = data
        self._event.set()

    def set_error(self, exc: Exception) -> None:
        self.error_exc = exc
        self._event.set()


# ---------------------------------------------------------------------------
# ReplyChunkAssembler — client-side chunked reply reassembly
# ---------------------------------------------------------------------------

class _ReplyChunkAssembler:
    """Reassembles chunked reply frames into a single result bytes.

    Uses ``mmap.mmap(-1, size)`` for deterministic OS-level release.
    """

    __slots__ = ('total_chunks', 'chunk_size', 'received', '_actual_total',
                 '_buf', '_received_flags')

    def __init__(
        self,
        total_chunks: int,
        chunk_size: int,
        *,
        max_total_chunks: int = 512,
        max_reassembly_bytes: int = 8 * (1 << 30),
    ) -> None:
        if total_chunks <= 0 or total_chunks > max_total_chunks:
            raise ValueError(
                f'total_chunks={total_chunks} out of range [1, {max_total_chunks}]'
            )
        alloc_size = total_chunks * chunk_size
        if alloc_size > max_reassembly_bytes:
            raise ValueError(
                f'Reassembly buffer {alloc_size} bytes exceeds '
                f'limit {max_reassembly_bytes}'
            )
        import mmap as _mmap
        self.total_chunks = total_chunks
        self.chunk_size = chunk_size
        self.received = 0
        self._actual_total = 0
        self._buf = _mmap.mmap(-1, alloc_size)
        self._received_flags = bytearray(total_chunks)

    def add(self, idx: int, data: bytes | memoryview) -> bool:
        """Write chunk data at the correct offset.  Returns True when complete."""
        if self._received_flags[idx]:
            return False
        offset = idx * self.chunk_size
        dlen = len(data)
        self._buf[offset:offset + dlen] = data
        self._received_flags[idx] = 1
        self._actual_total += dlen
        self.received += 1
        return self.received == self.total_chunks

    def assemble(self) -> bytes:
        """Return reassembled payload and release the mmap.

        .. note::

           ``buf.read()`` creates a ``bytes`` copy while the mmap is still
           alive, so peak RSS briefly doubles (mmap + bytes).  This is
           inherent to any copy-out scheme and acceptable given that the
           mmap is released immediately after via ``close()`` → ``munmap``.
        """
        buf = self._buf
        self._buf = None
        buf.seek(0)
        result = buf.read(self._actual_total)
        buf.close()
        return result

    def discard(self) -> None:
        """Release the mmap without assembling."""
        if self._buf is not None:
            self._buf.close()
            self._buf = None


# ---------------------------------------------------------------------------
# SharedClient
# ---------------------------------------------------------------------------

class SharedClient:
    """Concurrent multiplexed IPC client.

    Thread-safe: multiple ICRM consumers may call :meth:`call` concurrently
    from different threads.  A single UDS connection and buddy pool is shared.
    """

    def __init__(
        self,
        server_address: str,
        ipc_config: IPCConfig | None = None,
    ):
        self._config = ipc_config or IPCConfig()
        self._address = server_address
        self.region_id = server_address.replace('ipc://', '')
        self._socket_path = _resolve_socket_path(self.region_id)

        # UDS connection — single socket shared across all callers.
        self._sock: _socket.socket | None = None

        # Buddy pool (owned by client, shared with server).
        self._buddy_pool = None
        self._seg_views: list[memoryview] = []
        self._seg_base_addrs: list[int] = []

        # Concurrency primitives.
        self._send_lock = threading.Lock()      # Protects sendall atomicity
        self._alloc_lock = threading.Lock()     # Protects buddy alloc + seg_views
        self._pending: dict[int, PendingCall] = {}
        self._pending_lock = threading.Lock()
        self._rid_counter = 0
        self._rid_lock = threading.Lock()

        # Receive thread.
        self._recv_thread: threading.Thread | None = None
        self._running = False
        self._closed = False
        self._close_lock = threading.Lock()

        # Negotiated during handshake.
        self._method_table: MethodTable | None = None
        self._name_tables: dict[str, MethodTable] = {}
        self._default_name = ''
        self._chunked_capable = False

    # ------------------------------------------------------------------
    # Connection lifecycle
    # ------------------------------------------------------------------

    def connect(self) -> None:
        """Establish UDS connection, buddy handshake, start recv thread."""
        if self._sock is not None:
            return
        self._sock = self._do_connect()
        self._do_buddy_handshake()
        # Switch to a short recv timeout so terminate() can stop the recv
        # thread promptly.  On macOS, socket.shutdown(SHUT_RDWR) from
        # another thread doesn't reliably unblock a blocked recv().
        self._sock.settimeout(0.1)
        self._running = True
        self._recv_thread = threading.Thread(
            target=self._recv_loop,
            name=f'c2-shared-recv-{self.region_id}',
            daemon=True,
        )
        self._recv_thread.start()

    def _do_connect(self) -> _socket.socket:
        sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        sock.settimeout(10.0)
        sock.connect(self._socket_path)
        return sock

    def _do_buddy_handshake(self) -> None:
        """Create buddy pool and perform handshake with server."""
        try:
            from c_two.buddy import BuddyPoolHandle, PoolConfig
        except ImportError:
            logger.warning('c_two.buddy not available, falling back to inline-only')
            return

        self._buddy_pool = BuddyPoolHandle(PoolConfig(
            segment_size=self._config.pool_segment_size,
            min_block_size=4096,
            max_segments=1,
            max_dedicated_segments=4,
        ))

        # Force creation of the first segment.
        dummy = self._buddy_pool.alloc(4096)
        seg_name = self._buddy_pool.segment_name(0)
        seg_size = self._config.pool_segment_size
        self._buddy_pool.free(dummy)

        if seg_name is None:
            logger.warning('Failed to get segment name after dummy alloc')
            return

        segments = [(seg_name, seg_size)]

        # Handshake (only supported protocol).
        handshake_payload = encode_client_handshake(
            segments, CAP_CALL | CAP_METHOD_IDX | CAP_CHUNKED,
        )
        handshake_frame = encode_frame(0, 1 << 2, handshake_payload)  # FLAG_HANDSHAKE
        self._sock.sendall(handshake_frame)

        header = _recv_exact(self._sock, 16)
        total_len, _rid, flags = FRAME_STRUCT.unpack(header)
        payload_len = total_len - 12
        payload = _recv_exact(self._sock, payload_len) if payload_len > 0 else b''

        if not (flags & (1 << 2)):
            raise error.CompoClientError('Server did not respond with handshake ACK')

        if len(payload) < 1 or payload[0] != HANDSHAKE_VERSION:
            raise error.CompoClientError('Server does not support handshake')

        hs = decode_handshake(payload)
        if not (hs.capability_flags & CAP_CALL):
            raise error.CompoClientError('Server does not support routed calls')

        self._chunked_capable = bool(hs.capability_flags & CAP_CHUNKED)
        # Build per-route method tables.
        for route in hs.routes:
            table = MethodTable()
            for m in route.methods:
                table.add(m.name, m.index)
            self._name_tables[route.name] = table
        # Default route + unified table for backward compat.
        if hs.routes:
            self._default_name = hs.routes[0].name
            self._method_table = self._name_tables[self._default_name]

        # Cache persistent memoryviews for each buddy segment.
        self._seg_views = []
        self._seg_base_addrs = []
        for seg_idx in range(self._buddy_pool.segment_count()):
            base_addr, data_size = self._buddy_pool.seg_data_info(seg_idx)
            mv = memoryview(
                (ctypes.c_char * data_size).from_address(base_addr)
            ).cast('B')
            self._seg_views.append(mv)
            self._seg_base_addrs.append(base_addr)

        logger.debug('Buddy handshake complete, pool segment: %s', seg_name)

    def terminate(self) -> None:
        """Shut down the client: stop recv thread, close socket, destroy pool."""
        with self._close_lock:
            if self._closed:
                return
            self._closed = True

        self._running = False

        # Close socket to unblock recv thread.
        if self._sock is not None:
            try:
                # Set a very short timeout so the recv thread's blocking recv()
                # returns quickly with a timeout error, allowing it to see
                # _running=False and exit.  On macOS, shutdown(SHUT_RDWR) on a
                # UDS doesn't reliably unblock a concurrent recv().
                self._sock.settimeout(0.05)
            except Exception:
                pass
            try:
                self._sock.shutdown(_socket.SHUT_RDWR)
            except Exception:
                pass
            try:
                self._sock.close()
            except Exception:
                pass
            self._sock = None

        # Wait for recv thread to exit.
        if self._recv_thread is not None and self._recv_thread.is_alive():
            self._recv_thread.join(timeout=1.0)
            self._recv_thread = None

        # Wake up all pending callers with error.
        with self._pending_lock:
            for pending in self._pending.values():
                pending.set_error(error.CompoClientError('Client terminated'))
            self._pending.clear()

        # Destroy buddy pool.
        self._seg_views = []
        self._seg_base_addrs = []
        if self._buddy_pool is not None:
            try:
                self._buddy_pool.destroy()
            except Exception:
                pass
            self._buddy_pool = None

    # ------------------------------------------------------------------
    # RPC call (thread-safe, concurrent)
    # ------------------------------------------------------------------

    def call(self, method_name: str, data: bytes | None = None, *, name: str | None = None) -> bytes:
        """Send a CRM_CALL and return the response payload.

        Thread-safe: multiple threads may call concurrently.  Each call
        blocks only its own thread until the matching response arrives.

        Uses control-plane routing (method index + pure SHM
        payload).  In v1 mode, uses standard wire format.

        Parameters
        ----------
        method_name:
            The method to invoke on the CRM.
        data:
            Serialized arguments payload.
        name:
            Target CRM routing name for multi-CRM.  If ``None``, uses the
            default.

        Returns ``bytes`` (always copied from SHM for safety).
        """
        if self._closed:
            raise error.CompoClientError('Client is closed')
        if self._sock is None:
            self.connect()

        args = data if data is not None else b''
        payload_size = payload_total_size(args)
        wire_size = payload_size

        # Allocate request ID (32-bit wrapping to match frame header).
        with self._rid_lock:
            rid = self._rid_counter
            self._rid_counter = (self._rid_counter + 1) & 0xFFFFFFFF

        # Register pending call.
        pending = PendingCall(rid)
        with self._pending_lock:
            self._pending[rid] = pending

        # Chunked transfer for large payloads.
        chunk_threshold = int(self._config.pool_segment_size * self._config.chunk_threshold_ratio)
        if wire_size > chunk_threshold:
            if not self._chunked_capable:
                with self._pending_lock:
                    self._pending.pop(rid, None)
                raise error.MemoryPressureError(
                    f'Payload {wire_size} bytes exceeds segment size '
                    f'and server does not support chunked transfer',
                )
            return self._call_chunked(rid, pending, method_name, args, wire_size, name=name)

        try:
            with self._send_lock:
                self._send_request(rid, method_name, args, wire_size, name=name)
        except Exception as exc:
            with self._pending_lock:
                self._pending.pop(rid, None)
            if isinstance(exc, error.CCBaseError):
                raise
            raise error.CompoClientError(f'IPC call failed: {exc}') from exc

        # Wait for response (no locks held).
        try:
            return pending.wait(timeout=self._config.call_timeout
                                if hasattr(self._config, 'call_timeout') else 30.0)
        except TimeoutError:
            with self._pending_lock:
                self._pending.pop(rid, None)
            raise

    def relay(self, event_bytes: bytes) -> bytes:
        """Relay raw wire bytes to the server and return the response."""
        if self._closed:
            raise error.CompoClientError('Client is closed')
        if self._sock is None:
            self.connect()

        wire_size = len(event_bytes)

        with self._rid_lock:
            rid = self._rid_counter
            self._rid_counter = (self._rid_counter + 1) & 0xFFFFFFFF

        pending = PendingCall(rid)
        with self._pending_lock:
            self._pending[rid] = pending

        try:
            with self._send_lock:
                self._send_relay(rid, event_bytes, wire_size)
        except Exception as exc:
            with self._pending_lock:
                self._pending.pop(rid, None)
            if isinstance(exc, error.CCBaseError):
                raise
            raise error.CompoClientError(f'IPC relay failed: {exc}') from exc

        return pending.wait(timeout=30.0)

    # ------------------------------------------------------------------
    # Buddy allocation helpers
    # ------------------------------------------------------------------

    def _try_buddy_alloc(self, size: int, label: str = '') -> tuple[object | None, memoryview | None]:
        """Try buddy allocation with backpressure handling.

        Returns ``(alloc, shm_buf)`` on success, or ``(None, None)`` when
        the caller should fall back to inline transport.

        Raises :class:`~c_two.error.MemoryPressureError` when buddy pool
        is exhausted *and* the payload exceeds ``max_frame_size``.
        """
        if size <= self._config.shm_threshold or self._buddy_pool is None:
            return None, None

        with self._alloc_lock:
            try:
                alloc = self._buddy_pool.alloc(size)
            except Exception:
                alloc = None

            if alloc is None:
                if size <= self._config.max_frame_size:
                    logger.debug('Buddy alloc failed for %d bytes, inline fallback%s', size, f' ({label})' if label else '')
                    return None, None
                raise error.MemoryPressureError(
                    f'Buddy pool exhausted: cannot allocate {size} bytes; '
                    f'payload exceeds inline limit ({self._config.max_frame_size})',
                )

            if alloc.is_dedicated:
                self._buddy_pool.free_at(alloc.seg_idx, alloc.offset, size, True)
                if size <= self._config.max_frame_size:
                    logger.debug('Buddy alloc dedicated for %d bytes, inline fallback%s', size, f' ({label})' if label else '')
                    return None, None
                raise error.MemoryPressureError(
                    f'Buddy pool exhausted (dedicated segment): cannot allocate {size} bytes; '
                    f'payload exceeds inline limit ({self._config.max_frame_size})',
                )

            seg_mv = self._seg_views[alloc.seg_idx]
            shm_buf = seg_mv[alloc.offset : alloc.offset + size]
            return alloc, shm_buf

    def _free_buddy(self, alloc: object, size: int) -> None:
        """Free a buddy allocation (called on send failure)."""
        with self._alloc_lock:
            self._buddy_pool.free_at(
                alloc.seg_idx, alloc.offset, size, alloc.is_dedicated,
            )

    # ------------------------------------------------------------------
    # Chunked send (large payloads)
    # ------------------------------------------------------------------

    def _call_chunked(
        self,
        rid: int,
        pending: PendingCall,
        method_name: str,
        data: bytes,
        total_size: int,
        *,
        name: str | None = None,
    ) -> bytes:
        """Send a large payload as multiple chunked frames.

        Each chunk independently allocates from the buddy pool and is sent
        under ``_send_lock`` per-frame (not per-sequence), allowing other
        RIDs to interleave.
        """
        chunk_size = self._config.pool_segment_size // 2
        n_chunks = math.ceil(total_size / chunk_size)

        route_name = name if name is not None else self._default_name
        table = self._name_tables.get(route_name, self._method_table)
        method_idx = table.index_of(method_name) if table else 0

        try:
            for i in range(n_chunks):
                start = i * chunk_size
                end = min(start + chunk_size, total_size)
                chunk_data = data[start:end]
                chunk_len = end - start

                alloc, shm_buf = self._try_buddy_alloc(chunk_len, f'chunk-{i}')

                if alloc is not None:
                    try:
                        shm_buf[:chunk_len] = chunk_data
                        frame = encode_buddy_chunked_call_frame(
                            rid, alloc.seg_idx, alloc.offset, chunk_len,
                            alloc.is_dedicated, i, n_chunks,
                            name=route_name, method_idx=method_idx,
                        )
                        with self._send_lock:
                            self._sock.sendall(frame)
                    except Exception:
                        self._free_buddy(alloc, chunk_len)
                        raise
                else:
                    # Inline fallback for this chunk.
                    frame = encode_inline_chunked_call_frame(
                        rid, i, n_chunks, chunk_data,
                        name=route_name, method_idx=method_idx,
                    )
                    with self._send_lock:
                        self._sock.sendall(frame)
        except Exception:
            with self._pending_lock:
                self._pending.pop(rid, None)
            raise

        timeout = self._config.call_timeout if hasattr(self._config, 'call_timeout') else 30.0
        return pending.wait(timeout=timeout)

    # ------------------------------------------------------------------
    # Send helpers (called under _send_lock)
    # ------------------------------------------------------------------

    def _send_request(self, rid: int, method_name: str, args: bytes, payload_size: int, *, name: str | None = None) -> None:
        """Encode and send a call frame. Called under _send_lock.

        SHM contains pure payload (no wire header).
        Method routing uses 2-byte index in the UDS inline control frame.
        """
        sock = self._sock
        if sock is None:
            raise error.CompoClientError('Not connected')

        route_name = name if name is not None else self._default_name
        table = self._name_tables.get(route_name, self._method_table)
        method_idx = table.index_of(method_name) if table else 0

        alloc, shm_buf = self._try_buddy_alloc(payload_size, 'call')

        if alloc is None:
            inline_data = b''.join(args) if isinstance(args, (list, tuple)) else args
            frame = encode_inline_call_frame(rid, route_name, method_idx, inline_data)
            sock.sendall(frame)
            return

        try:
            if isinstance(args, (list, tuple)):
                off = 0
                for part in args:
                    part_len = len(part)
                    shm_buf[off:off + part_len] = part
                    off += part_len
            else:
                shm_buf[:payload_size] = args
            frame = encode_buddy_call_frame(
                rid, alloc.seg_idx, alloc.offset,
                payload_size, alloc.is_dedicated,
                route_name, method_idx,
            )
            sock.sendall(frame)
        except Exception:
            self._free_buddy(alloc, payload_size)
            raise

    def _send_relay(self, rid: int, event_bytes: bytes, wire_size: int) -> None:
        """Encode and send a relay frame. Called under _send_lock."""
        sock = self._sock
        if sock is None:
            raise error.CompoClientError('Not connected')

        alloc, shm_buf = self._try_buddy_alloc(wire_size, 'relay')

        if alloc is None:
            frame = encode_frame(rid, 0, event_bytes)
            sock.sendall(frame)
            return

        try:
            shm_buf[:wire_size] = event_bytes
            frame = encode_buddy_call_frame(
                rid, alloc.seg_idx, alloc.offset,
                wire_size, alloc.is_dedicated,
            )
            sock.sendall(frame)
        except Exception:
            self._free_buddy(alloc, wire_size)
            raise

    # ------------------------------------------------------------------
    # Background receive loop
    # ------------------------------------------------------------------

    def _recv_loop(self) -> None:
        """Background thread: read response frames, dispatch to pending callers."""
        sock = self._sock
        reply_assemblers: dict[int, _ReplyChunkAssembler] = {}
        try:
            while self._running and sock is not None:
                try:
                    header = _recv_exact(sock, 16)
                except _socket.timeout:
                    # Short timeout expired — re-check _running flag.
                    continue
                except (ConnectionResetError, BrokenPipeError, OSError):
                    break

                total_len, request_id, flags = FRAME_STRUCT.unpack(header)
                if total_len < 12:
                    continue
                payload_len = total_len - 12

                try:
                    payload = _recv_exact(sock, payload_len) if payload_len > 0 else b''
                except (ConnectionResetError, BrokenPipeError, OSError):
                    break

                # Chunked reply: accumulate in assembler.
                if flags & FLAG_CHUNKED:
                    try:
                        result, complete = self._handle_chunked_reply(
                            flags, payload, request_id, reply_assemblers,
                        )
                    except Exception as exc:
                        with self._pending_lock:
                            pending = self._pending.pop(request_id, None)
                        if pending is not None:
                            pending.set_error(exc)
                        continue

                    if not complete:
                        continue

                    with self._pending_lock:
                        pending = self._pending.pop(request_id, None)
                    if pending is not None:
                        pending.set_result(result)
                    continue

                # Decode response and dispatch.
                try:
                    result, err = self._decode_response(flags, payload, bool(flags & FLAG_BUDDY))
                except Exception as exc:
                    # Dispatch error to pending caller.
                    with self._pending_lock:
                        pending = self._pending.pop(request_id, None)
                    if pending is not None:
                        pending.set_error(exc)
                    continue

                with self._pending_lock:
                    pending = self._pending.pop(request_id, None)

                if pending is None:
                    # No matching caller (timed out or cancelled).
                    continue

                if err is not None:
                    pending.set_error(err)
                else:
                    pending.set_result(result)

        except (ConnectionResetError, BrokenPipeError, OSError):
            logger.debug('recv_loop: connection lost')
        except Exception:
            logger.error('recv_loop: unexpected error', exc_info=True)
        finally:
            # Clean up stale reply assemblers.
            for asm in reply_assemblers.values():
                asm.discard()
            reply_assemblers.clear()
            # Wake up any remaining pending callers.
            with self._pending_lock:
                for p in self._pending.values():
                    p.set_error(error.CompoClientError('Connection closed'))
                self._pending.clear()

    def _decode_response(
        self, flags: int, payload: bytes, is_buddy: bool,
    ) -> tuple[bytes | None, Exception | None]:
        """Decode a reply frame (FLAG_REPLY set)."""
        if is_buddy:
            # Buddy: [11B buddy_ptr][1B status][optional error]
            seg_idx, data_offset, data_size, is_dedicated, free_offset, free_size = decode_buddy_payload(payload)

            if self._buddy_pool is None:
                return None, error.CompoClientError('Buddy response but no pool')
            if seg_idx >= len(self._seg_views):
                return None, error.CompoClientError(f'Invalid seg_idx {seg_idx}')

            # Parse reply control (after buddy pointer).
            from ..ipc.buddy import BUDDY_PAYLOAD_STRUCT
            ctrl_offset = BUDDY_PAYLOAD_STRUCT.size
            # Check for reuse flag (19 bytes total buddy payload).
            bp_flags = payload[10] if len(payload) > 10 else 0
            if bp_flags & 0x02:  # BUDDY_REUSE_FLAG
                ctrl_offset = 19  # 11 + 8 bytes
            status, err_data, _consumed = decode_reply_control(payload, ctrl_offset)

            if status == STATUS_ERROR:
                # Free buddy block (may be empty allocation).
                with self._alloc_lock:
                    try:
                        self._buddy_pool.free_at(seg_idx, free_offset, free_size, is_dedicated)
                    except Exception:
                        pass
                if err_data:
                    err = error.CCError.deserialize(err_data)
                    if err:
                        return None, err
                return b'', None

            # Success: read pure payload from SHM.
            seg_mv = self._seg_views[seg_idx]
            if data_offset + data_size > len(seg_mv):
                return None, error.CompoClientError('Response out of bounds')
            result = bytes(seg_mv[data_offset : data_offset + data_size])

            with self._alloc_lock:
                try:
                    self._buddy_pool.free_at(seg_idx, free_offset, free_size, is_dedicated)
                except Exception:
                    logger.warning('Failed to free buddy block', exc_info=True)

            return result, None
        else:
            # Inline reply: [1B status][optional error | inline data]
            if len(payload) < 1:
                return b'', None
            status, err_data, consumed = decode_reply_control(payload, 0)
            if status == STATUS_ERROR:
                if err_data:
                    err = error.CCError.deserialize(memoryview(err_data))
                    if err:
                        return None, err
                return b'', None
            # Success: inline data follows control.
            result = bytes(payload[consumed:]) if consumed < len(payload) else b''
            return result, None

    # ------------------------------------------------------------------
    # Chunked reply reassembly
    # ------------------------------------------------------------------

    def _handle_chunked_reply(
        self,
        flags: int,
        payload: bytes,
        request_id: int,
        assemblers: dict[int, '_ReplyChunkAssembler'],
    ) -> tuple[bytes, bool]:
        """Process one chunked reply frame.

        Returns ``(result_bytes, complete)``.  When ``complete`` is False,
        ``result_bytes`` is meaningless.
        """
        is_buddy = bool(flags & FLAG_BUDDY)

        if is_buddy:
            from ..ipc.buddy import BUDDY_PAYLOAD_STRUCT as _BP
            seg_idx, data_offset, data_size, is_dedicated, free_offset, free_size = decode_buddy_payload(payload)
            if self._buddy_pool is None or seg_idx >= len(self._seg_views):
                raise error.CompoClientError(f'Chunked reply: invalid seg_idx {seg_idx}')
            seg_mv = self._seg_views[seg_idx]
            chunk_data = bytes(seg_mv[data_offset:data_offset + data_size])
            with self._alloc_lock:
                try:
                    self._buddy_pool.free_at(seg_idx, free_offset, free_size, is_dedicated)
                except Exception:
                    pass
            ctrl_off = _BP.size
        else:
            ctrl_off = 0

        chunk_idx, total_chunks, ch_consumed = decode_chunk_header(payload, ctrl_off)
        ctrl_off += ch_consumed

        if chunk_idx == 0:
            # First chunk: parse reply control (status byte).
            from ..wire import decode_reply_control as _drc
            status, err_data, rc_consumed = _drc(payload, ctrl_off)
            ctrl_off += rc_consumed
            if status == STATUS_ERROR:
                if err_data:
                    err = error.CCError.deserialize(err_data)
                    if err:
                        raise err
                raise error.CompoClientError('Chunked reply error (empty)')

            if not is_buddy:
                chunk_data = bytes(payload[ctrl_off:])

            chunk_size = self._config.pool_segment_size // 2
            asm = _ReplyChunkAssembler(
                total_chunks=total_chunks,
                chunk_size=chunk_size,
                max_total_chunks=self._config.max_total_chunks,
                max_reassembly_bytes=self._config.max_reassembly_bytes,
            )
            assemblers[request_id] = asm
        else:
            asm = assemblers.get(request_id)
            if asm is None:
                logger.warning('Orphan chunked reply chunk (rid=%d, idx=%d)',
                               request_id, chunk_idx)
                return b'', False
            if not is_buddy:
                chunk_data = bytes(payload[ctrl_off:])

        complete = asm.add(chunk_idx, chunk_data)

        if not complete:
            return b'', False

        del assemblers[request_id]
        return asm.assemble(), True

    # ------------------------------------------------------------------
    # Static utility methods
    # ------------------------------------------------------------------

    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        """Ping a server to check if it is alive."""
        region_id = server_address.replace('ipc://', '')
        socket_path = _resolve_socket_path(region_id)
        if not os.path.exists(socket_path):
            return False
        sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        try:
            sock.settimeout(timeout)
            sock.connect(socket_path)
            frame = encode_frame(0, FLAG_SIGNAL, PING_BYTES)
            sock.sendall(frame)
            header = _recv_exact(sock, 16)
            total_len, _rid, _flags = FRAME_STRUCT.unpack(header)
            payload_len = total_len - 12
            payload = _recv_exact(sock, payload_len) if payload_len > 0 else b''
            return len(payload) == 1 and payload[0] == MsgType.PONG
        except Exception:
            return False
        finally:
            sock.close()

    @staticmethod
    def shutdown(server_address: str, timeout: float = 0.5) -> bool:
        """Send shutdown signal to a server."""
        region_id = server_address.replace('ipc://', '')
        socket_path = _resolve_socket_path(region_id)
        if not os.path.exists(socket_path):
            return True
        sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        try:
            sock.settimeout(timeout)
            sock.connect(socket_path)
            frame = encode_frame(0, FLAG_SIGNAL, SHUTDOWN_CLIENT_BYTES)
            sock.sendall(frame)
            header = _recv_exact(sock, 16)
            total_len, _rid, _flags = FRAME_STRUCT.unpack(header)
            payload_len = total_len - 12
            if payload_len > 0:
                _recv_exact(sock, payload_len)
            return True
        except Exception:
            return False
        finally:
            sock.close()


# ---------------------------------------------------------------------------
# Module-level helpers
# ---------------------------------------------------------------------------

def _recv_exact(sock: _socket.socket, n: int) -> bytes:
    """Read exactly *n* bytes from *sock*."""
    if n == 0:
        return b''
    data = sock.recv(n)
    if len(data) == n:
        return data
    if not data:
        raise ConnectionResetError('Server closed connection')
    buf = bytearray(data)
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionResetError('Server closed connection')
        buf.extend(chunk)
    return bytes(buf)
