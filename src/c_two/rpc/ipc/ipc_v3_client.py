"""IPC v3 client — synchronous UDS client with buddy-allocated SHM data plane.

Request-response: the client sends a request and blocks until the server
replies, giving serial request-response semantics. The client creates a
shared buddy pool that the server opens. Both allocate/free blocks
concurrently via the SHM-based spinlock.

Ownership model (consumer frees):
- Request blocks: client allocs, server frees via free_at after reading
- Response blocks: server allocs, client frees via free_at after reading
"""

from __future__ import annotations

import ctypes
import ctypes.util
import logging
import os
import socket as _socket
import struct
import threading
import time
from pathlib import Path

from ... import error
from ..event.msg_type import MsgType
from ..base.base_client import BaseClient
from ..util.adaptive_buffer import AdaptiveBuffer
from ..util.wire import (
    write_call_into,
    call_wire_size,
    payload_total_size,
    decode,
    get_call_header_cache,
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
    U32_STRUCT,
    encode_frame,
    decode_frame,
    encode_inline_call_frame,
)
from .ipc_v3_protocol import (
    FLAG_BUDDY,
    decode_buddy_payload,
    encode_buddy_handshake,
    decode_buddy_handshake,
    encode_buddy_call_frame,
)

logger = logging.getLogger(__name__)

_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')

# Pre-computed constants for inline CRM_REPLY parsing (avoids decode() overhead).
_CRM_REPLY_TYPE = int(MsgType.CRM_REPLY)
_U32_LE = struct.Struct('<I')

# Warm response buffer: use memmove to pre-faulted buffer for large responses
# to avoid page-fault overhead from fresh bytes() allocation (4-5x faster for >= 1GB).
_WARM_BUF_THRESHOLD = 1 * 1024 * 1024  # 1MB

_libc = ctypes.CDLL(ctypes.util.find_library('c'))
_libc.memmove.restype = ctypes.c_void_p
_libc.memmove.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_size_t]
_libc.memset.restype = ctypes.c_void_p
_libc.memset.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_size_t]
_memmove = _libc.memmove
_memset = _libc.memset


def _resolve_socket_path(region_id: str) -> str:
    sock_dir = Path(_IPC_SOCK_DIR)
    return str(sock_dir / f'{region_id}.sock')


class IPCv3Client(BaseClient):
    """Synchronous IPC v3 client with buddy pool SHM data plane."""

    def __init__(self, server_address: str, ipc_config: IPCConfig | None = None):
        super().__init__(server_address)
        self._config = ipc_config or IPCConfig()
        self.region_id = server_address.replace('ipc-v3://', '')
        self._socket_path = _resolve_socket_path(self.region_id)
        self._sock: _socket.socket | None = None
        self._conn_lock = threading.Lock()
        self._next_rid: int = 0

        # Buddy pool (owned by client, shared with server).
        self._buddy_pool = None  # c2_buddy.BuddyPoolHandle
        self._pool_handshake_done = False
        self._seg_views: list[memoryview] = []  # Cached segment data region views
        self._seg_base_addrs: list[int] = []    # Cached segment data base addresses

        # Warm response buffer: pre-allocated bytearray with touched pages
        # to avoid page-fault overhead on large buddy response copies.
        self._response_buf: bytearray | None = None
        self._response_buf_addr: int = 0

        # Deferred free: for scatter-reuse responses, keep the SHM block
        # alive until the next call so the caller gets a zero-copy
        # memoryview into SHM (no warm-buffer memmove).
        self._deferred_response_free: tuple | None = None

    # ------------------------------------------------------------------
    # Connection management
    # ------------------------------------------------------------------

    def _connect(self) -> _socket.socket:
        sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        sock.settimeout(10.0)
        sock.connect(self._socket_path)
        return sock

    def _ensure_connection(self) -> _socket.socket:
        sock = self._sock
        if sock is not None:
            if not self._pool_handshake_done and self._config.pool_enabled:
                self._do_buddy_handshake()
            return sock
        # Slow path: connect + handshake outside _conn_lock to avoid blocking
        # other threads during network I/O (10s timeout, SHM allocation).
        new_sock = self._connect()
        self._pool_handshake_done = False
        self._sock = new_sock
        if self._config.pool_enabled:
            self._do_buddy_handshake()
        return new_sock

    def _close_connection(self) -> None:
        # Flush deferred free before closing pool.
        deferred = self._deferred_response_free
        if deferred is not None:
            self._deferred_response_free = None
            self._free_buddy_response(deferred)
        if self._sock is not None:
            try:
                self._sock.close()
            except Exception:
                pass
            self._sock = None
        self._pool_handshake_done = False
        self._seg_views = []
        self._seg_base_addrs = []
        if self._buddy_pool is not None:
            try:
                self._buddy_pool.destroy()
            except Exception:
                pass
            self._buddy_pool = None

    # ------------------------------------------------------------------
    # Buddy pool handshake
    # ------------------------------------------------------------------

    def _do_buddy_handshake(self) -> None:
        """Create buddy pool and exchange segment info with server."""
        try:
            import c2_buddy
        except ImportError:
            logger.warning('c2_buddy not available, falling back to inline-only')
            self._pool_handshake_done = True
            return

        self._buddy_pool = c2_buddy.BuddyPoolHandle(c2_buddy.PoolConfig(
            segment_size=self._config.pool_segment_size,
            min_block_size=4096,
            max_segments=1,
            max_dedicated_segments=4,
        ))

        # Force creation of the first segment with a dummy alloc/free.
        dummy = self._buddy_pool.alloc(4096)
        seg_name = self._buddy_pool.segment_name(0)
        seg_size = self._config.pool_segment_size
        self._buddy_pool.free(dummy)

        if seg_name is None:
            logger.warning('Failed to get segment name after dummy alloc')
            self._pool_handshake_done = True
            return

        # Send handshake frame.
        handshake_payload = encode_buddy_handshake([(seg_name, seg_size)])
        handshake_frame = encode_frame(0, 1 << 2, handshake_payload)  # FLAG_HANDSHAKE
        self._sock.sendall(handshake_frame)

        # Read ACK: 16-byte header + payload.
        header = self._recv_exact(16)
        total_len, _rid, flags = FRAME_STRUCT.unpack(header)
        payload_len = total_len - 12
        payload = self._recv_exact(payload_len) if payload_len > 0 else b''

        if not (flags & (1 << 2)):  # FLAG_HANDSHAKE
            logger.warning('Expected handshake ACK, got flags=%d', flags)

        # Cache a persistent memoryview and base address for each buddy segment data region.
        self._seg_views = []
        self._seg_base_addrs = []
        for seg_idx in range(self._buddy_pool.segment_count()):
            base_addr, data_size = self._buddy_pool.seg_data_info(seg_idx)
            mv = memoryview(
                (ctypes.c_char * data_size).from_address(base_addr)
            ).cast('B')
            self._seg_views.append(mv)
            self._seg_base_addrs.append(base_addr)

        self._pool_handshake_done = True
        logger.debug('Buddy handshake complete, pool segment: %s', seg_name)

    # ------------------------------------------------------------------
    # RPC call
    # ------------------------------------------------------------------

    def _ensure_response_buf(self, needed: int) -> None:
        """Ensure warm response buffer is large enough with pages pre-faulted."""
        if self._response_buf is not None and len(self._response_buf) >= needed:
            return
        self._response_buf = bytearray(needed)
        addr = ctypes.addressof(
            (ctypes.c_char * needed).from_buffer(self._response_buf)
        )
        _memset(addr, 0, needed)
        self._response_buf_addr = addr

    def call(self, method_name: str, data: bytes | None = None) -> bytes | memoryview:
        """Send a CRM_CALL and return the response payload.

        Returns bytes for inline/small responses. For large buddy responses
        (>= 1 MB), returns a memoryview into a client-owned warm buffer
        (safe to hold — not backed by SHM).
        """
        args = data if data is not None else b''
        method_bytes = method_name.encode('utf-8')
        wire_size = call_wire_size(len(method_bytes), payload_total_size(args))

        with self._conn_lock:
            # Flush any deferred buddy free from the previous call.
            deferred = self._deferred_response_free
            if deferred is not None:
                self._deferred_response_free = None
                self._free_buddy_response(deferred)

            sock = self._ensure_connection()
            request_id = self._next_rid
            self._next_rid += 1

            try:
                if wire_size <= self._config.shm_threshold or self._buddy_pool is None:
                    # Flatten tuple payloads for inline frame encoder.
                    inline_args = b''.join(args) if isinstance(args, (list, tuple)) else args
                    frame = encode_inline_call_frame(
                        request_id, method_name, inline_args, get_call_header_cache(),
                    )
                    sock.sendall(frame)
                else:
                    alloc = self._buddy_pool.alloc(wire_size)
                    if alloc.is_dedicated:
                        self._buddy_pool.free_at(
                            alloc.seg_idx, alloc.offset, wire_size, True,
                        )
                        inline_args = b''.join(args) if isinstance(args, (list, tuple)) else args
                        frame = encode_inline_call_frame(
                            request_id, method_name, inline_args, get_call_header_cache(),
                        )
                        sock.sendall(frame)
                    else:
                        seg_mv = self._seg_views[alloc.seg_idx]
                        shm_buf = seg_mv[alloc.offset : alloc.offset + wire_size]
                        try:
                            write_call_into(shm_buf, 0, method_name, args)
                            frame = encode_buddy_call_frame(
                                request_id, alloc.seg_idx, alloc.offset,
                                wire_size, alloc.is_dedicated,
                            )
                            sock.sendall(frame)
                        except Exception:
                            self._buddy_pool.free_at(alloc.seg_idx, alloc.offset, wire_size, alloc.is_dedicated)
                            raise
                    # Note: server frees the request block after reading (consumer frees).

                # Read response.
                response_data, free_info = self._recv_response()

            except error.CCBaseError:
                raise
            except Exception as exc:
                self._close_connection()
                raise error.CompoClientError(f'IPC v3 call failed: {exc}') from exc

        # Decode wire message outside lock.
        if free_info is not None:
            # Buddy zero-copy fast path: parse CRM_REPLY header inline to avoid
            # decode() overhead (Envelope allocation, MsgType enum lookup, memoryview wrapping).
            mv = response_data
            total_size = len(mv)
            if total_size < 5 or mv[0] != _CRM_REPLY_TYPE:
                self._free_buddy_response(free_info)
                raise error.CompoClientError(f'Unexpected buddy response type: 0x{mv[0]:02x}')
            err_len = _U32_LE.unpack_from(mv, 1)[0]
            if err_len > 0:
                err_end = 5 + err_len
                if err_end > total_size:
                    self._free_buddy_response(free_info)
                    raise error.CompoClientError(
                        f'Corrupted CRM_REPLY: err_len={err_len} exceeds data size={total_size}')
                err_bytes = bytes(mv[5:err_end])
                self._free_buddy_response(free_info)
                err = error.CCError.deserialize(err_bytes)
                if err:
                    raise err
                return b''
            data_size = total_size - 5
            if data_size == 0:
                self._free_buddy_response(free_info)
                return b''
            if data_size >= _WARM_BUF_THRESHOLD:
                # Deferred free: return memoryview directly into SHM,
                # defer the buddy block free until the next call().
                # This eliminates the warm buffer memmove copy.
                self._deferred_response_free = free_info
                return mv[5:]
            payload = bytes(mv[5:])
            self._free_buddy_response(free_info)
            return payload
        else:
            env = decode(response_data)
            if env.msg_type != MsgType.CRM_REPLY:
                raise error.CompoClientError(f'Unexpected response type: {env.msg_type}')
            if env.error:
                err = error.CCError.deserialize(env.error)
                if err:
                    raise err
            return env.payload if env.payload is not None else b''

    def relay(self, event_bytes: bytes) -> bytes:
        """Relay raw event bytes to the server."""
        wire_size = len(event_bytes)

        with self._conn_lock:
            sock = self._ensure_connection()
            request_id = self._next_rid
            self._next_rid += 1

            try:
                if wire_size <= self._config.shm_threshold or self._buddy_pool is None:
                    frame = encode_frame(request_id, 0, event_bytes)
                    sock.sendall(frame)
                else:
                    alloc = self._buddy_pool.alloc(wire_size)
                    if alloc.is_dedicated:
                        self._buddy_pool.free_at(
                            alloc.seg_idx, alloc.offset, wire_size, True,
                        )
                        frame = encode_frame(request_id, 0, event_bytes)
                        sock.sendall(frame)
                    else:
                        seg_mv = self._seg_views[alloc.seg_idx]
                        shm_buf = seg_mv[alloc.offset : alloc.offset + wire_size]
                        try:
                            shm_buf[:wire_size] = event_bytes
                            frame = encode_buddy_call_frame(
                                request_id, alloc.seg_idx, alloc.offset,
                                wire_size, alloc.is_dedicated,
                            )
                            sock.sendall(frame)
                        except Exception:
                            self._buddy_pool.free_at(alloc.seg_idx, alloc.offset, wire_size, alloc.is_dedicated)
                            raise

                response_data, free_info = self._recv_response()
            except error.CCBaseError:
                raise
            except Exception as exc:
                self._close_connection()
                raise error.CompoClientError(f'IPC v3 relay failed: {exc}') from exc

        # For buddy zero-copy responses, materialize to bytes before freeing.
        if free_info is not None:
            if isinstance(response_data, memoryview):
                response_data = bytes(response_data)
            self._free_buddy_response(free_info)
        return response_data

    def terminate(self) -> None:
        with self._conn_lock:
            self._close_connection()

    # ------------------------------------------------------------------
    # Response handling
    # ------------------------------------------------------------------

    def _recv_response(self) -> tuple[bytes | memoryview, tuple | None]:
        """Read a response frame and return (data, buddy_free_info).

        For buddy frames: data is a zero-copy memoryview into SHM,
        buddy_free_info is (seg_idx, free_offset, free_size, is_dedicated).
        For inline frames: data is bytes, buddy_free_info is None.
        The caller must free the buddy block after consuming the data.
        """
        while True:
            header = self._recv_exact(16)
            total_len, request_id, flags = FRAME_STRUCT.unpack(header)
            if total_len < 12:
                raise error.EventDeserializeError(f'Frame too small: {total_len}')
            if total_len > self._config.max_frame_size:
                raise error.EventDeserializeError(f'Frame too large: {total_len}')
            payload_len = total_len - 12
            payload = self._recv_exact(payload_len) if payload_len > 0 else b''

            if flags & FLAG_BUDDY:
                seg_idx, data_offset, data_size, is_dedicated, free_offset, free_size = decode_buddy_payload(payload)
                if self._buddy_pool is None:
                    raise error.CompoClientError('Buddy response but no pool')
                if seg_idx >= len(self._seg_views):
                    raise error.CompoClientError(f'Invalid seg_idx {seg_idx} from server (max {len(self._seg_views)-1})')
                seg_mv = self._seg_views[seg_idx]
                if data_offset + data_size > len(seg_mv):
                    raise error.CompoClientError(f'Server response out of bounds: offset={data_offset} size={data_size} seg_len={len(seg_mv)}')
                data_mv = seg_mv[data_offset : data_offset + data_size]
                return data_mv, (seg_idx, data_offset, data_size, free_offset, free_size, is_dedicated)
            else:
                # Skip PONG frames (heartbeat responses).
                if len(payload) >= 1 and payload[0] == MsgType.PONG:
                    continue
                return payload, None

    def _free_buddy_response(self, free_info: tuple | None) -> None:
        """Free a buddy response block if applicable."""
        if free_info is not None:
            seg_idx, _data_off, _data_sz, free_offset, free_size, is_dedicated = free_info
            try:
                self._buddy_pool.free_at(seg_idx, free_offset, free_size, is_dedicated)
            except Exception:
                logger.warning('Failed to free buddy block seg=%d off=%d size=%d',
                               seg_idx, free_offset, free_size, exc_info=True)

    # ------------------------------------------------------------------
    # Low-level I/O
    # ------------------------------------------------------------------

    def _recv_exact(self, n: int) -> bytes:
        data = self._sock.recv(n)
        if len(data) == n:
            return data
        if not data:
            raise ConnectionResetError('Server closed connection')
        buf = bytearray(data)
        while len(buf) < n:
            chunk = self._sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionResetError('Server closed connection')
            buf.extend(chunk)
        return bytes(buf)

    # ------------------------------------------------------------------
    # Static methods (BaseClient interface)
    # ------------------------------------------------------------------

    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        region_id = server_address.replace('ipc-v3://', '')
        socket_path = _resolve_socket_path(region_id)
        if not os.path.exists(socket_path):
            return False
        sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        try:
            sock.settimeout(timeout)
            sock.connect(socket_path)
            frame = encode_frame(0, 0, PING_BYTES)
            sock.sendall(frame)
            header = _recv_exact_sock(sock, 16)
            total_len, _rid, _flags = FRAME_STRUCT.unpack(header)
            payload_len = total_len - 12
            payload = _recv_exact_sock(sock, payload_len) if payload_len > 0 else b''
            env = decode(payload)
            return env.msg_type == MsgType.PONG
        except Exception:
            return False
        finally:
            sock.close()

    @staticmethod
    def shutdown(server_address: str, timeout: float = 0.5) -> bool:
        region_id = server_address.replace('ipc-v3://', '')
        socket_path = _resolve_socket_path(region_id)
        if not os.path.exists(socket_path):
            return True
        sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        try:
            sock.settimeout(timeout)
            sock.connect(socket_path)
            frame = encode_frame(0, 0, SHUTDOWN_CLIENT_BYTES)
            sock.sendall(frame)
            header = _recv_exact_sock(sock, 16)
            total_len, _rid, _flags = FRAME_STRUCT.unpack(header)
            payload_len = total_len - 12
            payload = _recv_exact_sock(sock, payload_len) if payload_len > 0 else b''
            env = decode(payload)
            return env.msg_type == MsgType.SHUTDOWN_ACK
        except Exception:
            return False
        finally:
            sock.close()

    @property
    def supports_direct_call(self) -> bool:
        return False


def _recv_exact_sock(sock: _socket.socket, n: int) -> bytes:
    """Receive exactly n bytes from a blocking socket."""
    data = bytearray()
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise ConnectionResetError('Server closed connection')
        data.extend(chunk)
    return bytes(data)
