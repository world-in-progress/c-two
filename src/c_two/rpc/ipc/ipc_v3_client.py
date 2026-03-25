"""IPC v3 client — synchronous UDS client with buddy-allocated SHM data plane.

Full-duplex: client creates a shared buddy pool, server opens it.
Both allocate/free blocks concurrently via the SHM-based spinlock.

Ownership model (consumer frees):
- Request blocks: client allocs, server frees via free_at after reading
- Response blocks: server allocs, client frees via free_at after reading
"""

from __future__ import annotations

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
from ..util.wire import (
    write_call_into,
    call_wire_size,
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

    # ------------------------------------------------------------------
    # Connection management
    # ------------------------------------------------------------------

    def _connect(self) -> _socket.socket:
        sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        sock.settimeout(10.0)
        sock.connect(self._socket_path)
        return sock

    def _ensure_connection(self) -> _socket.socket:
        if self._sock is None:
            self._sock = self._connect()
            self._pool_handshake_done = False
        if not self._pool_handshake_done and self._config.pool_enabled:
            self._do_buddy_handshake()
        return self._sock

    def _close_connection(self) -> None:
        if self._sock is not None:
            try:
                self._sock.close()
            except Exception:
                pass
            self._sock = None
        self._pool_handshake_done = False
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
            max_segments=self._config.max_pool_segments,
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

        self._pool_handshake_done = True
        logger.debug('Buddy handshake complete, pool segment: %s', seg_name)

    # ------------------------------------------------------------------
    # RPC call
    # ------------------------------------------------------------------

    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        """Send a CRM_CALL and return the response payload."""
        args = data if data is not None else b''
        method_bytes = method_name.encode('utf-8')
        wire_size = call_wire_size(len(method_bytes), len(args))

        with self._conn_lock:
            sock = self._ensure_connection()
            request_id = self._next_rid
            self._next_rid += 1

            try:
                if wire_size <= self._config.shm_threshold or self._buddy_pool is None:
                    frame = encode_inline_call_frame(
                        request_id, method_name, args, get_call_header_cache(),
                    )
                    sock.sendall(frame)
                else:
                    alloc = self._buddy_pool.alloc(wire_size)
                    wire_buf = bytearray(wire_size)
                    write_call_into(wire_buf, 0, method_name, args)
                    self._buddy_pool.write(alloc, bytes(wire_buf))

                    frame = encode_buddy_call_frame(
                        request_id, alloc.seg_idx, alloc.offset,
                        wire_size, alloc.is_dedicated,
                    )
                    sock.sendall(frame)
                    # Note: server frees the request block after reading (consumer frees).

                # Read response.
                response_bytes = self._recv_response()

            except error.CCBaseError:
                raise
            except Exception as exc:
                self._close_connection()
                raise error.CompoClientError(f'IPC v3 call failed: {exc}') from exc

        # Decode wire message outside lock.
        env = decode(response_bytes)
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
                    self._buddy_pool.write(alloc, event_bytes)
                    frame = encode_buddy_call_frame(
                        request_id, alloc.seg_idx, alloc.offset,
                        wire_size, alloc.is_dedicated,
                    )
                    sock.sendall(frame)

                response_bytes = self._recv_response()
            except error.CCBaseError:
                raise
            except Exception as exc:
                self._close_connection()
                raise error.CompoClientError(f'IPC v3 relay failed: {exc}') from exc

        return bytes(response_bytes)

    def terminate(self) -> None:
        with self._conn_lock:
            self._close_connection()

    # ------------------------------------------------------------------
    # Response handling
    # ------------------------------------------------------------------

    def _recv_response(self) -> bytes | memoryview:
        """Read a response frame and return decoded wire bytes."""
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
                seg_idx, offset, data_size, is_dedicated = decode_buddy_payload(payload)
                if self._buddy_pool is None:
                    raise error.CompoClientError('Buddy response but no pool')
                # Consumer reads and frees (client frees response blocks).
                data = self._buddy_pool.read_at(seg_idx, offset, data_size, is_dedicated)
                try:
                    self._buddy_pool.free_at(seg_idx, offset, data_size, is_dedicated)
                except Exception:
                    pass
                return data
            else:
                # Skip PONG frames (heartbeat responses).
                if len(payload) >= 1 and payload[0] == MsgType.PONG:
                    continue
                return payload

    # ------------------------------------------------------------------
    # Low-level I/O
    # ------------------------------------------------------------------

    def _recv_exact(self, n: int) -> bytes:
        data = bytearray()
        while len(data) < n:
            chunk = self._sock.recv(n - len(data))
            if not chunk:
                raise ConnectionResetError('Server closed connection')
            data.extend(chunk)
        return bytes(data)

    # ------------------------------------------------------------------
    # Static methods (BaseClient interface)
    # ------------------------------------------------------------------

    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        region_id = server_address.replace('ipc-v3://', '')
        socket_path = _resolve_socket_path(region_id)
        if not os.path.exists(socket_path):
            return False
        try:
            sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
            sock.settimeout(timeout)
            sock.connect(socket_path)
            frame = encode_frame(0, 0, PING_BYTES)
            sock.sendall(frame)
            header = _recv_exact_sock(sock, 16)
            total_len, _rid, _flags = FRAME_STRUCT.unpack(header)
            payload_len = total_len - 12
            payload = _recv_exact_sock(sock, payload_len) if payload_len > 0 else b''
            sock.close()
            env = decode(payload)
            return env.msg_type == MsgType.PONG
        except Exception:
            return False

    @staticmethod
    def shutdown(server_address: str, timeout: float = 0.5) -> bool:
        region_id = server_address.replace('ipc-v3://', '')
        socket_path = _resolve_socket_path(region_id)
        if not os.path.exists(socket_path):
            return True
        try:
            sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
            sock.settimeout(timeout)
            sock.connect(socket_path)
            frame = encode_frame(0, 0, SHUTDOWN_CLIENT_BYTES)
            sock.sendall(frame)
            header = _recv_exact_sock(sock, 16)
            total_len, _rid, _flags = FRAME_STRUCT.unpack(header)
            payload_len = total_len - 12
            payload = _recv_exact_sock(sock, payload_len) if payload_len > 0 else b''
            sock.close()
            env = decode(payload)
            return env.msg_type == MsgType.SHUTDOWN_ACK
        except Exception:
            return False

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
