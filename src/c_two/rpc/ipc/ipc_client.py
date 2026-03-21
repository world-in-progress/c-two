"""
IPC v2 Client — connects to IPCv2Server via UDS control plane.

Uses raw synchronous sockets with persistent connection for minimal latency.
SharedMemory data plane for large payloads with ownership transfer:
client creates SHM → sends reference → server takes ownership and releases.
"""

import logging
import os
import socket as _socket
import struct
import tempfile
import threading
import uuid
from multiprocessing import shared_memory

from ... import error
from ..base import BaseClient
from ..event import Event, EventTag
from ..util.adaptive_buffer import AdaptiveBuffer
from ..util.encoding import add_length_prefix, parse_message
from .ipc_server import (
    IPCConfig,
    _FLAG_SHM,
    _decode_frame,
    _encode_frame,
    _fast_read_shm,
    _scatter_write_event_multi_to_shm,
    _shm_name,
)

logger = logging.getLogger(__name__)


def _write_shm(name: str, data: bytes) -> shared_memory.SharedMemory:
    shm = shared_memory.SharedMemory(name=name, create=True, size=len(data))
    shm.buf[:len(data)] = data
    return shm


def _resolve_socket_path(region_id: str) -> str:
    tmpdir = os.getenv('IPC_V2_SOCKET_DIR', tempfile.gettempdir())
    return os.path.join(tmpdir, f'cc_ipcv2_{region_id}.sock')


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


def _recv_frame_sync(sock: _socket.socket, max_frame_size: int = 0) -> bytes:
    header = _recv_exact(sock, 4)
    total_len = struct.unpack('<I', header)[0]

    # S2: reject oversized or undersized frames before allocation
    if total_len < 8:
        raise error.EventDeserializeError(
            f'Frame too small: total_len={total_len} (minimum 8)'
        )
    if max_frame_size > 0 and total_len > max_frame_size:
        raise error.EventDeserializeError(
            f'Frame too large: total_len={total_len} exceeds max_frame_size={max_frame_size}'
        )

    body = _recv_exact(sock, total_len)
    return header + body


class IPCv2Client(BaseClient):

    def __init__(self, server_address: str, ipc_config: IPCConfig | None = None):
        super().__init__(server_address)
        self._config = ipc_config or IPCConfig()
        self.region_id = server_address.replace('ipc-v2://', '')
        self._socket_path = _resolve_socket_path(self.region_id)
        self._sock: _socket.socket | None = None
        self._conn_lock = threading.Lock()
        self._read_buf: AdaptiveBuffer = AdaptiveBuffer()  # adaptive buffer for SHM reads

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
        return sock

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

    # ------------------------------------------------------------------
    # Core send/recv — raw synchronous socket, persistent connection
    # ------------------------------------------------------------------

    def _send_and_recv(self, request_id: str, flags: int, payload: bytes) -> tuple[str, int, bytes]:
        with self._conn_lock:
            frame = _encode_frame(request_id, flags, payload)
            for attempt in range(2):
                try:
                    sock = self._ensure_connection()
                    _send_frame_sync(sock, frame)
                    raw = _recv_frame_sync(sock, self._config.max_frame_size)
                    return _decode_frame(raw)
                except (ConnectionError, BrokenPipeError, OSError):
                    self._close_connection()
                    if attempt == 1:
                        raise
            raise error.CompoClientError('IPC v2 connection failed after retry')

    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        self._read_buf.maybe_decay()

        request_id = str(uuid.uuid4())
        method_bytes = method_name.encode('utf-8')
        args = data if data is not None else b''
        flags = 0

        # Estimate serialized Event size (tag ~8B + prefixes 32B + method + args)
        estimated_event_size = 48 + len(method_bytes) + len(args)

        if estimated_event_size >= self._config.shm_threshold:
            # OPT-5: Scatter-write request Event directly to SHM (1 copy of args)
            shm_name = _shm_name(self.region_id, request_id, 'req')
            try:
                request_shm, written = _scatter_write_event_multi_to_shm(
                    shm_name, EventTag.CRM_CALL, [method_bytes, args]
                )
                request_shm.close()  # server takes ownership
            except Exception as e:
                raise error.CompoSerializeInput(f'Error writing request to SHM: {e}')
            size_header = struct.pack('<Q', written)
            payload = shm_name.encode('utf-8') + b'\x00' + size_header
            flags |= _FLAG_SHM
        else:
            # Inline path
            try:
                combined = add_length_prefix(method_bytes) + add_length_prefix(args)
                event = Event(tag=EventTag.CRM_CALL, data=combined, request_id=request_id)
                payload = event.serialize()
            except Exception as e:
                raise error.CompoSerializeInput(f'Error occurred when serializing request: {e}')

        try:
            resp_rid, resp_flags, resp_payload = self._send_and_recv(request_id, flags, payload)
        except Exception as exc:
            raise error.CompoClientError(f'IPC v2 call failed: {exc}') from exc

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
            response_bytes, self._read_buf = _fast_read_shm(shm_name, size, self._read_buf)
        else:
            response_bytes = resp_payload

        response_event = Event.deserialize(response_bytes)
        if response_event.tag != EventTag.CRM_REPLY:
            raise error.CompoClientError(f'Unexpected response tag: {response_event.tag}')

        sub_responses = parse_message(response_event.data)
        if len(sub_responses) != 2:
            raise error.CompoDeserializeOutput(
                f'Expected exactly 2 sub-messages (error and result), got {len(sub_responses)}'
            )

        err = error.CCError.deserialize(sub_responses[0])
        if err:
            raise err

        return sub_responses[1]

    def relay(self, event_bytes: bytes) -> bytes:
        request_id = str(uuid.uuid4())
        flags = 0

        if len(event_bytes) >= self._config.shm_threshold:
            shm_name = _shm_name(self.region_id, request_id, 'relay')
            shm = _write_shm(shm_name, event_bytes)
            shm.close()
            size_header = struct.pack('<Q', len(event_bytes))
            payload = shm_name.encode('utf-8') + b'\x00' + size_header
            flags |= _FLAG_SHM
        else:
            payload = event_bytes

        try:
            resp_rid, resp_flags, resp_payload = self._send_and_recv(request_id, flags, payload)
        except Exception as exc:
            raise error.CompoClientError(f'IPC v2 relay failed: {exc}') from exc

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

            request_id = str(uuid.uuid4())
            ping_event = Event(tag=EventTag.PING, request_id=request_id)
            frame = _encode_frame(request_id, 0, ping_event.serialize())

            _send_frame_sync(sock, frame)
            raw = _recv_frame_sync(sock)
            _, _, resp_payload = _decode_frame(raw)
            resp_event = Event.deserialize(resp_payload)

            sock.close()
            return resp_event.tag == EventTag.PONG
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

            request_id = str(uuid.uuid4())
            shutdown_event = Event(tag=EventTag.SHUTDOWN_FROM_CLIENT, request_id=request_id)
            frame = _encode_frame(request_id, 0, shutdown_event.serialize())

            _send_frame_sync(sock, frame)
            raw = _recv_frame_sync(sock)
            _, _, resp_payload = _decode_frame(raw)
            resp_event = Event.deserialize(resp_payload)

            sock.close()
            return resp_event.tag == EventTag.SHUTDOWN_ACK
        except Exception:
            return False
