"""
IPC v2 Client — connects to IPCv2Server via UDS control plane.

Uses raw synchronous sockets with persistent connection for minimal latency.
SharedMemory data plane for large payloads with pool-based pre-allocation
(Phase 1) or per-request ownership transfer (legacy fallback).
"""

import hashlib
import itertools
import logging
import os
import socket as _socket
import struct
import tempfile
import threading
import time
from multiprocessing import shared_memory

from ... import error
from ..base import BaseClient
from ..event.msg_type import MsgType
from ..util.adaptive_buffer import AdaptiveBuffer
from ..util.wire import encode_call, decode, call_wire_size, PING_BYTES, SHUTDOWN_CLIENT_BYTES
from .ipc_server import (
    DEFAULT_MAX_FRAME_SIZE,
    IPCConfig,
    _FLAG_HANDSHAKE,
    _FLAG_POOL,
    _FLAG_SHM,
    _encode_frame,
    _fast_read_shm,
    _read_from_pool_shm,
    _shm_name,
)
from .shm_pool import (
    close_pool_shm,
    create_pool_shm,
    decode_handshake,
    encode_handshake,
)

logger = logging.getLogger(__name__)

# Module-level atomic counter for unique SHM names across concurrent clients
_pool_id_counter = itertools.count(1)


def _write_shm(name: str, data: bytes) -> shared_memory.SharedMemory:
    shm = shared_memory.SharedMemory(name=name, create=True, size=len(data))
    shm.buf[:len(data)] = data
    return shm


def _resolve_socket_path(region_id: str) -> str:
    tmpdir = os.getenv('IPC_V2_SOCKET_DIR', tempfile.gettempdir())
    return os.path.join(tmpdir, f'cc_ipcv2_{region_id}.sock')


def _client_pool_shm_name(region_id: str) -> str:
    """Generate a globally unique SHM name for a client's request pool segment."""
    uid = next(_pool_id_counter)
    raw = f'{region_id}_cpid{os.getpid()}_c{uid}'.encode()
    h = hashlib.md5(raw).hexdigest()[:12]
    return f'ccpr_{h}'


def _recv_exact(sock: _socket.socket, n: int) -> bytes:
    """Read exactly *n* bytes from a blocking socket."""
    buf = bytearray(n)
    view = memoryview(buf)
    pos = 0
    while pos < n:
        nbytes = sock.recv_into(view[pos:])
        if nbytes == 0:
            raise ConnectionError('Connection closed by server')
        pos += nbytes
    return bytes(buf)


def _send_frame_sync(sock: _socket.socket, frame: bytes) -> None:
    sock.sendall(frame)


def _recv_frame_sync(sock: _socket.socket, max_frame_size: int = DEFAULT_MAX_FRAME_SIZE) -> tuple[int, int, bytes]:
    """Read a frame, returning (request_id, flags, payload) directly."""
    header = _recv_exact(sock, 16)  # 4B total_len + 8B rid + 4B flags
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
    payload = _recv_exact(sock, payload_len) if payload_len > 0 else b''
    return request_id, flags, payload


class IPCv2Client(BaseClient):

    def __init__(self, server_address: str, ipc_config: IPCConfig | None = None):
        super().__init__(server_address)
        self._config = ipc_config or IPCConfig()
        self.region_id = server_address.replace('ipc-v2://', '')
        self._socket_path = _resolve_socket_path(self.region_id)
        self._sock: _socket.socket | None = None
        self._conn_lock = threading.Lock()
        self._read_buf: AdaptiveBuffer = AdaptiveBuffer()  # adaptive buffer for SHM reads
        self._next_rid: int = 0

        # Pool SHM state (set during handshake, cleared on disconnect)
        # Unified: single SHM for both request writes and response reads
        self._pool_shm: shared_memory.SharedMemory | None = None  # we created (owner)
        self._pool_segment_size: int = 0
        self._pool_last_used: float = 0.0  # monotonic timestamp of last pool access

    # ------------------------------------------------------------------
    # Persistent connection management
    # ------------------------------------------------------------------

    def _connect(self) -> _socket.socket:
        sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        try:
            sock.connect(self._socket_path)
        except Exception:
            sock.close()
            raise

        # Attempt pool handshake if enabled
        if self._config.pool_enabled:
            try:
                self._do_pool_handshake(sock)
            except Exception as exc:
                logger.warning('IPC v2 pool handshake failed, using fallback: %s', exc)
                self._cleanup_pool()

        return sock

    def _do_pool_handshake(self, sock: _socket.socket) -> None:
        """Exchange pool SHM metadata with the server (unified bidirectional)."""
        seg_size = self._config.pool_segment_size

        # Create our pool SHM segment (used for both directions)
        shm_name = _client_pool_shm_name(self.region_id)
        self._pool_shm = create_pool_shm(shm_name, seg_size)

        # Send handshake frame
        hs_payload = encode_handshake(shm_name, seg_size)
        hs_frame = _encode_frame(0, _FLAG_HANDSHAKE, hs_payload)
        _send_frame_sync(sock, hs_frame)

        # Receive server's ACK (empty name → unified mode)
        _, resp_flags, resp_payload = _recv_frame_sync(sock, self._config.max_frame_size)
        if not (resp_flags & _FLAG_HANDSHAKE):
            raise error.CompoClientError('Expected pool handshake response from server')

        _, resp_seg_size = decode_handshake(resp_payload)
        self._pool_segment_size = min(seg_size, resp_seg_size)
        self._pool_last_used = time.monotonic()

    def _ensure_connection(self) -> _socket.socket:
        if self._sock is not None:
            return self._sock
        self._sock = self._connect()
        return self._sock

    def _close_connection(self) -> None:
        if self._sock is not None:
            try:
                self._sock.close()
            except Exception:
                pass
            self._sock = None
        self._cleanup_pool()

    def _cleanup_pool(self) -> None:
        """Release pool SHM resources."""
        close_pool_shm(self._pool_shm, unlink=True)  # we created it
        self._pool_shm = None
        self._pool_segment_size = 0
        self._pool_last_used = 0.0

    def _maybe_decay_pool(self) -> None:
        """Tear down pool SHM if idle beyond pool_decay_seconds.

        Called at the start of call()/relay(). If the pool has been idle
        too long, we release the SHM now. A subsequent large-payload call
        will trigger a re-handshake via _ensure_pool().
        """
        decay = self._config.pool_decay_seconds
        if decay <= 0 or self._pool_shm is None:
            return
        if time.monotonic() - self._pool_last_used > decay:
            logger.debug('Pool SHM idle > %.0fs, tearing down', decay)
            self._cleanup_pool()

    def _ensure_pool(self, sock: _socket.socket) -> bool:
        """Re-handshake to rebuild the pool if it was decayed.

        Returns True if pool is available after this call.
        """
        if self._pool_shm is not None:
            return True
        if not self._config.pool_enabled:
            return False
        try:
            self._do_pool_handshake(sock)
            return True
        except Exception as exc:
            logger.warning('IPC v2 pool re-handshake failed: %s', exc)
            self._cleanup_pool()
            return False

    # ------------------------------------------------------------------
    # Core send/recv — raw synchronous socket, persistent connection
    # ------------------------------------------------------------------

    def _send_recv_locked(self, request_id: int, flags: int, payload: bytes) -> tuple[int, int, bytes]:
        """Send frame and receive response. Caller MUST hold _conn_lock."""
        frame = _encode_frame(request_id, flags, payload)
        for attempt in range(2):
            try:
                sock = self._ensure_connection()
                _send_frame_sync(sock, frame)
                return _recv_frame_sync(sock, self._config.max_frame_size)
            except (ConnectionError, BrokenPipeError, OSError):
                self._close_connection()
                if attempt == 1:
                    raise
        raise error.CompoClientError('IPC v2 connection failed after retry')

    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        self._read_buf.maybe_decay()

        method_bytes = method_name.encode('utf-8')
        args = data if data is not None else b''
        estimated_wire_size = call_wire_size(len(method_bytes), len(args))

        with self._conn_lock:
            self._maybe_decay_pool()

            request_id = self._next_rid
            self._next_rid += 1

            # Lazy re-handshake if pool was decayed but a large call needs it
            if (
                estimated_wire_size >= self._config.shm_threshold
                and self._pool_shm is None
                and self._config.pool_enabled
                and self._sock is not None
            ):
                self._ensure_pool(self._sock)

            flags = 0

            if (
                estimated_wire_size >= self._config.shm_threshold
                and self._pool_shm is not None
                and estimated_wire_size <= self._pool_segment_size
            ):
                # Pool path: write into pre-allocated SHM (zero syscalls)
                try:
                    from ..util.wire import write_call_into
                    write_call_into(self._pool_shm.buf, 0, method_name, args)
                except Exception as e:
                    raise error.CompoSerializeInput(f'Error writing request to pool SHM: {e}')
                payload = struct.pack('<Q', estimated_wire_size)
                flags |= _FLAG_POOL
                self._pool_last_used = time.monotonic()
            elif estimated_wire_size >= self._config.shm_threshold:
                # Fallback: per-request SHM (legacy path)
                shm_name = _shm_name(self.region_id, str(request_id), 'req')
                try:
                    from ..util.wire import write_call_into
                    shm = shared_memory.SharedMemory(name=shm_name, create=True, size=estimated_wire_size)
                    write_call_into(shm.buf, 0, method_name, args)
                    shm.close()
                except Exception as e:
                    raise error.CompoSerializeInput(f'Error writing request to SHM: {e}')
                size_header = struct.pack('<Q', estimated_wire_size)
                payload = shm_name.encode('utf-8') + b'\x00' + size_header
                flags |= _FLAG_SHM
            else:
                try:
                    payload = encode_call(method_name, args)
                except Exception as e:
                    raise error.CompoSerializeInput(f'Error occurred when serializing request: {e}')

            try:
                resp_rid, resp_flags, resp_payload = self._send_recv_locked(request_id, flags, payload)
            except Exception as exc:
                raise error.CompoClientError(f'IPC v2 call failed: {exc}') from exc

            # Read response (pool read must also be under lock)
            if resp_flags & _FLAG_POOL:
                if self._pool_shm is None or len(resp_payload) < 8:
                    raise error.EventDeserializeError(
                        'Pool SHM response received but no pool handshake was done'
                    )
                size = struct.unpack('<Q', resp_payload[:8])[0]
                if size > self._pool_segment_size:
                    raise error.EventDeserializeError(
                        f'Pool response size {size} exceeds segment size {self._pool_segment_size}'
                    )
                response_bytes, self._read_buf = _read_from_pool_shm(
                    self._pool_shm.buf, size, self._read_buf,
                )
            elif resp_flags & _FLAG_SHM:
                parts = resp_payload.split(b'\x00', 1)
                if len(parts) != 2 or len(parts[1]) < 8:
                    raise error.EventDeserializeError('Malformed SHM reference in response frame')
                shm_name = parts[0].decode('utf-8')
                size = struct.unpack('<Q', parts[1])[0]
                if size > self._config.max_payload_size:
                    raise error.EventDeserializeError(
                        f'SHM payload size {size} exceeds limit {self._config.max_payload_size}'
                    )
                response_bytes, self._read_buf = _fast_read_shm(shm_name, size, self._read_buf)
            else:
                response_bytes = resp_payload

        # Decode outside lock (CPU-bound, no shared mutable state)
        env = decode(response_bytes)
        if env.msg_type != MsgType.CRM_REPLY:
            raise error.CompoClientError(f'Unexpected response type: {env.msg_type}')

        if env.error:
            err = error.CCError.deserialize(env.error)
            if err:
                raise err

        return env.payload if env.payload is not None else b''

    def relay(self, event_bytes: bytes) -> bytes:
        data_len = len(event_bytes)

        with self._conn_lock:
            self._maybe_decay_pool()

            request_id = self._next_rid
            self._next_rid += 1
            flags = 0

            # Lazy re-handshake if pool was decayed but a large relay needs it
            if (
                data_len >= self._config.shm_threshold
                and self._pool_shm is None
                and self._config.pool_enabled
                and self._sock is not None
            ):
                self._ensure_pool(self._sock)

            if (
                data_len >= self._config.shm_threshold
                and self._pool_shm is not None
                and data_len <= self._pool_segment_size
            ):
                # Pool path: write raw bytes into pre-allocated SHM
                self._pool_shm.buf[:data_len] = event_bytes
                payload = struct.pack('<Q', data_len)
                flags |= _FLAG_POOL
                self._pool_last_used = time.monotonic()
            elif data_len >= self._config.shm_threshold:
                shm_name = _shm_name(self.region_id, str(request_id), 'relay')
                shm = _write_shm(shm_name, event_bytes)
                shm.close()
                size_header = struct.pack('<Q', data_len)
                payload = shm_name.encode('utf-8') + b'\x00' + size_header
                flags |= _FLAG_SHM
            else:
                payload = event_bytes

            try:
                resp_rid, resp_flags, resp_payload = self._send_recv_locked(request_id, flags, payload)
            except Exception as exc:
                raise error.CompoClientError(f'IPC v2 relay failed: {exc}') from exc

            if resp_flags & _FLAG_POOL:
                if self._pool_shm is None or len(resp_payload) < 8:
                    raise error.EventDeserializeError(
                        'Pool SHM response received but no pool handshake was done'
                    )
                size = struct.unpack('<Q', resp_payload[:8])[0]
                data, self._read_buf = _read_from_pool_shm(
                    self._pool_shm.buf, size, self._read_buf,
                )
                return bytes(data)

            if resp_flags & _FLAG_SHM:
                parts = resp_payload.split(b'\x00', 1)
                if len(parts) != 2 or len(parts[1]) < 8:
                    raise error.EventDeserializeError('Malformed SHM reference in response frame')
                shm_name = parts[0].decode('utf-8')
                size = struct.unpack('<Q', parts[1])[0]
                if size > self._config.max_payload_size:
                    raise error.EventDeserializeError(
                        f'SHM payload size {size} exceeds limit {self._config.max_payload_size}'
                    )
                data, self._read_buf = _fast_read_shm(shm_name, size, self._read_buf)
                return bytes(data)

            return resp_payload

    def terminate(self) -> None:
        self._close_connection()
        self._read_buf.release()

    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        region_id = server_address.replace('ipc-v2://', '')
        socket_path = _resolve_socket_path(region_id)

        if not os.path.exists(socket_path):
            return False

        try:
            sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
            sock.settimeout(timeout)
            sock.connect(socket_path)

            request_id = 0
            frame = _encode_frame(request_id, 0, PING_BYTES)

            _send_frame_sync(sock, frame)
            _, _, resp_payload = _recv_frame_sync(sock)
            env = decode(resp_payload)

            sock.close()
            return env.msg_type == MsgType.PONG
        except Exception:
            return False

    @staticmethod
    def shutdown(server_address: str, timeout: float = 0.5) -> bool:
        region_id = server_address.replace('ipc-v2://', '')
        socket_path = _resolve_socket_path(region_id)

        if not os.path.exists(socket_path):
            return True

        try:
            sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
            sock.settimeout(timeout)
            sock.connect(socket_path)

            request_id = 0
            frame = _encode_frame(request_id, 0, SHUTDOWN_CLIENT_BYTES)

            _send_frame_sync(sock, frame)
            _, _, resp_payload = _recv_frame_sync(sock)
            env = decode(resp_payload)

            sock.close()
            return env.msg_type == MsgType.SHUTDOWN_ACK
        except Exception:
            return False
