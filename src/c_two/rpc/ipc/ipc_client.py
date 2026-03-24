"""
IPC v2 Client — connects to IPCv2Server via UDS control plane.

Uses raw synchronous sockets with persistent connection for minimal latency.
SharedMemory data plane for large payloads with pool-based pre-allocation
or per-request ownership transfer (legacy fallback).

Split pool architecture:
    - Client Outbound Pool: client creates, writes requests; server reads
    - Server Response Pool: server creates, writes responses; client reads
    - Each side manages their own pool independently
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
from ..util.wire import decode, call_wire_size, write_call_into, PING_BYTES, PONG_BYTES, SHUTDOWN_CLIENT_BYTES, get_call_header_cache
from .ipc_protocol import (
    FLAG_SHM, FLAG_POOL, FLAG_HANDSHAKE, FLAG_CTRL,
    FRAME_STRUCT, U64_STRUCT, FRAME_HEADER_SIZE, POOL_PAYLOAD_HEADER_SIZE,
    CTRL_SEGMENT_ANNOUNCE, CTRL_CONSUMED,
    POOL_DIR_OUTBOUND, POOL_DIR_RESPONSE,
    IPCConfig, DEFAULT_MAX_FRAME_SIZE,
    encode_frame, encode_inline_call_frame,
    encode_ctrl_segment_announce, decode_ctrl_segment_announce,
    encode_ctrl_consumed, decode_ctrl_consumed,
    fast_read_shm, read_from_pool_shm, shm_name,
)
from .shm_pool import (
    close_pool_shm,
    create_pool_shm,
    decode_handshake,
    decode_handshake_ack,
    encode_handshake,
)

logger = logging.getLogger(__name__)

# Module-level atomic counter for unique SHM names across concurrent clients
_pool_id_counter = itertools.count(1)

# Pre-built PONG frame for heartbeat auto-reply (allocated once)
_PONG_FRAME = encode_frame(0, 0, PONG_BYTES)


def _write_bytes_into(buf, offset: int, data: bytes) -> None:
    """Write raw *data* into *buf* starting at *offset*."""
    end = offset + len(data)
    buf[offset:end] = data


def _resolve_socket_path(region_id: str) -> str:
    tmpdir = os.getenv('IPC_V2_SOCKET_DIR', tempfile.gettempdir())
    return os.path.join(tmpdir, f'cc_ipcv2_{region_id}.sock')


def _client_pool_shm_name(region_id: str) -> str:
    """Generate a globally unique SHM name for a client's outbound pool segment.

    Format: ``ccpo{pid_hex}_{hash_hex}`` — always exactly 20 chars.
    """
    pid_hex = format(os.getpid(), 'x')
    uid = next(_pool_id_counter)
    raw = f'{region_id}_cpid{os.getpid()}_c{uid}'.encode()
    hash_len = 15 - len(pid_hex)
    h = hashlib.md5(raw).hexdigest()[:hash_len]
    return f'ccpo{pid_hex}_{h}'


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


_HEADER_SIZE = FRAME_HEADER_SIZE  # 16 bytes


def _recv_frame_sync(sock: _socket.socket, max_frame_size: int = DEFAULT_MAX_FRAME_SIZE) -> tuple[int, int, bytes]:
    """Read a frame, returning (request_id, flags, payload) directly.

    If the server sends a PING probe, automatically replies with PONG
    and loops to read the actual response frame.  CTRL frames are
    returned as-is for the caller to handle (not consumed here).
    """
    while True:
        # Read header directly into a reusable-pattern buffer (avoids _recv_exact alloc + bytes copy)
        hdr = bytearray(_HEADER_SIZE)
        hdr_view = memoryview(hdr)
        pos = 0
        while pos < _HEADER_SIZE:
            nbytes = sock.recv_into(hdr_view[pos:])
            if nbytes == 0:
                raise ConnectionError('Connection closed by server')
            pos += nbytes
        total_len, request_id, flags = FRAME_STRUCT.unpack(hdr)

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

        # Auto-reply to server heartbeat PINGs
        if payload == PING_BYTES:
            _send_frame_sync(sock, _PONG_FRAME)
            continue

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

        # Client outbound pool (client creates, writes requests)
        self._outbound_pool_shms: list[shared_memory.SharedMemory] = []
        self._outbound_pool_sizes: list[int] = []
        self._outbound_pool_names: list[str] = []
        self._outbound_budget_used: int = 0
        self._outbound_last_used: float = 0.0

        # Server response pool (server creates, client reads responses)
        self._resp_pool_shms: list[shared_memory.SharedMemory] = []
        self._resp_pool_sizes: list[int] = []

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
        """Exchange split pool SHM metadata with the server (v3 handshake)."""
        seg_size = self._config.pool_segment_size

        # Create client outbound pool segment[0]
        pool_name = _client_pool_shm_name(self.region_id)
        pool_shm = create_pool_shm(pool_name, seg_size)

        # Send v3 handshake frame (segment_index=0)
        hs_payload = encode_handshake(pool_name, seg_size, 0)
        hs_frame = encode_frame(0, FLAG_HANDSHAKE, hs_payload)
        _send_frame_sync(sock, hs_frame)

        # Receive server's v3 ACK (carries server response pool info)
        _, resp_flags, resp_payload = _recv_frame_sync(sock, self._config.max_frame_size)
        if not (resp_flags & FLAG_HANDSHAKE):
            raise error.CompoClientError('Expected pool handshake response from server')

        _, client_neg_size, server_seg_size, server_pool_name = decode_handshake_ack(resp_payload)
        negotiated_size = min(seg_size, client_neg_size)

        self._outbound_pool_shms = [pool_shm]
        self._outbound_pool_sizes = [negotiated_size]
        self._outbound_pool_names = [pool_name]
        self._outbound_budget_used = negotiated_size
        self._outbound_last_used = time.monotonic()

        # Open server's response pool (read-only from client's perspective)
        if server_pool_name and server_seg_size > 0:
            try:
                resp_shm = shared_memory.SharedMemory(
                    name=server_pool_name, create=False, track=False,
                )
                actual_resp_size = min(server_seg_size, resp_shm.size)
                self._resp_pool_shms = [resp_shm]
                self._resp_pool_sizes = [actual_resp_size]
            except Exception as exc:
                logger.warning('Failed to open server response pool %r: %s', server_pool_name, exc)
                self._resp_pool_shms = []
                self._resp_pool_sizes = []

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
        """Release all pool SHM resources."""
        # Unlink outbound pool (client owns these)
        for shm in self._outbound_pool_shms:
            close_pool_shm(shm, unlink=True)
        self._outbound_pool_shms.clear()
        self._outbound_pool_sizes.clear()
        self._outbound_pool_names.clear()
        self._outbound_budget_used = 0
        self._outbound_last_used = 0.0

        # Close response pool handles (server owns and unlinks these)
        for shm in self._resp_pool_shms:
            close_pool_shm(shm)
        self._resp_pool_shms.clear()
        self._resp_pool_sizes.clear()

    def _maybe_decay_pool(self) -> None:
        """Tear down outbound pool SHM if idle beyond pool_decay_seconds.

        Decays tail segments first (keeps segment[0] always).
        """
        decay = self._config.pool_decay_seconds
        if decay <= 0 or not self._outbound_pool_shms:
            return
        if time.monotonic() - self._outbound_last_used > decay:
            while len(self._outbound_pool_shms) > 1:
                shm = self._outbound_pool_shms.pop()
                size = self._outbound_pool_sizes.pop()
                self._outbound_pool_names.pop()
                self._outbound_budget_used -= size
                close_pool_shm(shm, unlink=True)
            logger.debug('Pool SHM idle > %.0fs, decayed tail segments', decay)

    def _ensure_pool(self, sock: _socket.socket) -> bool:
        """Re-handshake to rebuild the pool if it was decayed.

        Returns True if pool is available after this call.
        """
        if self._outbound_pool_shms:
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

    def _handle_ctrl(self, payload: bytes) -> None:
        """Process a control message from the server."""
        if len(payload) < 1:
            return
        ctrl_type = payload[0]

        if ctrl_type == CTRL_SEGMENT_ANNOUNCE:
            direction, index, size, name = decode_ctrl_segment_announce(payload)
            if direction != POOL_DIR_RESPONSE:
                logger.warning(
                    'Client received SEGMENT_ANNOUNCE for direction=%d (expected response)',
                    direction,
                )
                return
            try:
                shm = shared_memory.SharedMemory(name=name, create=False, track=False)
                actual_size = min(size, shm.size)
                if index != len(self._resp_pool_shms):
                    logger.warning(
                        'SEGMENT_ANNOUNCE index mismatch: expected %d, got %d',
                        len(self._resp_pool_shms), index,
                    )
                    shm.close()
                    return
                self._resp_pool_shms.append(shm)
                self._resp_pool_sizes.append(actual_size)
            except Exception as exc:
                logger.warning('Failed to open announced response segment %r: %s', name, exc)

        elif ctrl_type == CTRL_CONSUMED:
            # Server consumed an outbound pool segment — in half-duplex this
            # is redundant but we accept it for protocol consistency.
            pass

    def _recv_response_sync(self, sock: _socket.socket) -> tuple[int, int, bytes]:
        """Receive a response frame, processing any CTRL frames in between."""
        while True:
            rid, flags, payload = _recv_frame_sync(sock, self._config.max_frame_size)
            if flags & FLAG_CTRL:
                self._handle_ctrl(payload)
                continue
            return rid, flags, payload

    # ------------------------------------------------------------------
    # Core send/recv — raw synchronous socket, persistent connection
    # ------------------------------------------------------------------

    def _send_frame_recv_locked(self, frame: bytes, flags: int = 0) -> tuple[int, int, bytes]:
        """Send a pre-built frame and receive response. Caller MUST hold _conn_lock."""
        max_attempts = 1 if (flags & FLAG_POOL) else 2
        for attempt in range(max_attempts):
            try:
                sock = self._ensure_connection()
                _send_frame_sync(sock, frame)
                return self._recv_response_sync(sock)
            except (ConnectionError, BrokenPipeError, OSError):
                self._close_connection()
                if attempt == max_attempts - 1:
                    raise
        raise error.CompoClientError('IPC v2 connection failed after retry')

    def _send_request(
        self,
        request_id: int,
        wire_size: int,
        inline_builder,
        shm_writer,
        shm_direction: str = 'req',
    ) -> tuple[int, int, bytes]:
        """Path selection, frame build, send, and receive.  Caller MUST hold _conn_lock.

        Parameters
        ----------
        request_id : int
            Monotonic request identifier.
        wire_size : int
            Size of the wire-encoded payload (determines inline vs SHM path).
        inline_builder : ``(request_id: int) -> bytes``
            Builds the complete frame for the inline path.
        shm_writer : ``(buf, offset: int) -> None``
            Writes wire data into a shared-memory buffer at *offset*.
        shm_direction : str
            SHM name suffix for the fallback per-request SHM path.
        """
        # Lazy re-handshake if pool was decayed but a large request needs it
        if (
            wire_size >= self._config.shm_threshold
            and not self._outbound_pool_shms
            and self._config.pool_enabled
            and self._sock is not None
        ):
            self._ensure_pool(self._sock)

        if wire_size >= self._config.shm_threshold and self._outbound_pool_shms:
            # Pool path: find an outbound segment that fits
            pool_used = False
            for idx, (shm, seg_size) in enumerate(
                zip(self._outbound_pool_shms, self._outbound_pool_sizes)
            ):
                if wire_size <= seg_size:
                    try:
                        shm_writer(shm.buf, 0)
                    except Exception as e:
                        raise error.CompoSerializeInput(
                            f'Error writing request to pool SHM: {e}',
                        )
                    payload = struct.pack('<BQ', idx, wire_size)
                    flags = FLAG_POOL
                    self._outbound_last_used = time.monotonic()
                    frame = encode_frame(request_id, flags, payload)
                    pool_used = True
                    break

            if not pool_used and len(self._outbound_pool_shms) < self._config.max_pool_segments:
                # Expand outbound pool: create new segment + announce via CTRL
                new_size = max(self._config.pool_segment_size, wire_size)
                # Check budget
                if self._outbound_budget_used + new_size <= self._config.max_pool_memory:
                    new_index = len(self._outbound_pool_shms)
                    try:
                        new_name = _client_pool_shm_name(self.region_id)
                        new_shm = create_pool_shm(new_name, new_size)
                        actual_size = min(new_size, new_shm.size)

                        self._outbound_pool_shms.append(new_shm)
                        self._outbound_pool_sizes.append(actual_size)
                        self._outbound_pool_names.append(new_name)
                        self._outbound_budget_used += actual_size

                        # Send CTRL_SEGMENT_ANNOUNCE to server (before the data frame)
                        sock = self._ensure_connection()
                        ctrl_payload = encode_ctrl_segment_announce(
                            POOL_DIR_OUTBOUND, new_index, actual_size, new_name,
                        )
                        ctrl_frame = encode_frame(0, FLAG_CTRL, ctrl_payload)
                        _send_frame_sync(sock, ctrl_frame)

                        # Use the new segment
                        if wire_size <= actual_size:
                            try:
                                shm_writer(new_shm.buf, 0)
                            except Exception as e:
                                raise error.CompoSerializeInput(
                                    f'Error writing request to pool SHM: {e}',
                                )
                            payload = struct.pack('<BQ', new_index, wire_size)
                            flags = FLAG_POOL
                            self._outbound_last_used = time.monotonic()
                            frame = encode_frame(request_id, flags, payload)
                            pool_used = True
                    except Exception as exc:
                        logger.warning('IPC v2 pool expansion failed: %s', exc)

            if pool_used:
                return self._send_frame_recv_locked(frame, flags)

        if wire_size >= self._config.shm_threshold:
            # Fallback: per-request SHM (legacy path)
            req_shm_name = shm_name(self.region_id, str(request_id), shm_direction)
            try:
                shm = shared_memory.SharedMemory(
                    name=req_shm_name, create=True, size=wire_size,
                )
                shm_writer(shm.buf, 0)
                shm.close()
            except Exception as e:
                raise error.CompoSerializeInput(
                    f'Error writing request to SHM: {e}',
                )
            size_header = U64_STRUCT.pack(wire_size)
            payload = req_shm_name.encode('utf-8') + b'\x00' + size_header
            flags = FLAG_SHM
            frame = encode_frame(request_id, flags, payload)
        else:
            # Inline path: single-alloc frame
            try:
                frame = inline_builder(request_id)
            except Exception as e:
                raise error.CompoSerializeInput(
                    f'Error occurred when serializing request: {e}',
                )
            flags = 0

        return self._send_frame_recv_locked(frame, flags)

    def _read_response_bytes(
        self, resp_flags: int, resp_payload: bytes,
    ) -> bytes | memoryview:
        """Decode response from server response pool, fallback SHM, or inline payload.

        Must be called under ``_conn_lock`` for pool-SHM safety.
        """
        if resp_flags & FLAG_POOL:
            if not self._resp_pool_shms or len(resp_payload) < POOL_PAYLOAD_HEADER_SIZE:
                raise error.EventDeserializeError(
                    'Pool SHM response received but no response pool available'
                )
            segment_index = resp_payload[0]
            size = U64_STRUCT.unpack(resp_payload[1:9])[0]
            if segment_index >= len(self._resp_pool_shms):
                raise error.EventDeserializeError(
                    f'Response pool segment_index {segment_index} out of range '
                    f'(have {len(self._resp_pool_shms)} segments)'
                )
            seg_size = self._resp_pool_sizes[segment_index]
            if size > seg_size:
                raise error.EventDeserializeError(
                    f'Pool response size {size} exceeds segment size {seg_size}'
                )
            data, self._read_buf = read_from_pool_shm(
                self._resp_pool_shms[segment_index].buf, size, self._read_buf,
            )
            return data

        if resp_flags & FLAG_SHM:
            parts = resp_payload.split(b'\x00', 1)
            if len(parts) != 2 or len(parts[1]) < 8:
                raise error.EventDeserializeError(
                    'Malformed SHM reference in response frame'
                )
            resp_shm_name = parts[0].decode('utf-8')
            size = U64_STRUCT.unpack(parts[1])[0]
            if size > self._config.max_payload_size:
                raise error.EventDeserializeError(
                    f'SHM payload size {size} exceeds limit '
                    f'{self._config.max_payload_size}'
                )
            data, self._read_buf = fast_read_shm(
                resp_shm_name, size, self._read_buf,
            )
            return data

        return resp_payload

    # ------------------------------------------------------------------
    # Public API — call / relay
    # ------------------------------------------------------------------

    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        self._read_buf.maybe_decay()

        method_bytes = method_name.encode('utf-8')
        args = data if data is not None else b''
        wire_size = call_wire_size(len(method_bytes), len(args))

        with self._conn_lock:
            self._maybe_decay_pool()
            request_id = self._next_rid
            self._next_rid += 1

            try:
                _, resp_flags, resp_payload = self._send_request(
                    request_id, wire_size,
                    inline_builder=lambda rid: encode_inline_call_frame(
                        rid, method_name, args, get_call_header_cache(),
                    ),
                    shm_writer=lambda buf, off: write_call_into(
                        buf, off, method_name, args,
                    ),
                )
            except error.CompoSerializeInput:
                raise
            except Exception as exc:
                raise error.CompoClientError(
                    f'IPC v2 call failed: {exc}',
                ) from exc

            response_bytes = self._read_response_bytes(resp_flags, resp_payload)

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
        wire_size = len(event_bytes)

        with self._conn_lock:
            self._maybe_decay_pool()
            request_id = self._next_rid
            self._next_rid += 1

            try:
                _, resp_flags, resp_payload = self._send_request(
                    request_id, wire_size,
                    inline_builder=lambda rid: encode_frame(rid, 0, event_bytes),
                    shm_writer=lambda buf, off: _write_bytes_into(
                        buf, off, event_bytes,
                    ),
                    shm_direction='relay',
                )
            except error.CompoSerializeInput:
                raise
            except Exception as exc:
                raise error.CompoClientError(
                    f'IPC v2 relay failed: {exc}',
                ) from exc

            response_bytes = self._read_response_bytes(resp_flags, resp_payload)

        return bytes(response_bytes)

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
            frame = encode_frame(request_id, 0, PING_BYTES)

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
            frame = encode_frame(request_id, 0, SHUTDOWN_CLIENT_BYTES)

            _send_frame_sync(sock, frame)
            _, _, resp_payload = _recv_frame_sync(sock)
            env = decode(resp_payload)

            sock.close()
            return env.msg_type == MsgType.SHUTDOWN_ACK
        except Exception:
            return False
