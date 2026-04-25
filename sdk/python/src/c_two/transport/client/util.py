"""IPC utility functions — lightweight server probes."""
from __future__ import annotations

import os
import socket as _socket
import struct

# Wire constants (copied from transport layer to avoid heavy imports)
_FRAME_STRUCT = struct.Struct('<IQI')   # total_len(4) + request_id(8) + flags(4)
_FLAG_SIGNAL = 1 << 11

# Signal bytes (must match MsgType enum values)
_PING = bytes([0x01])            # MsgType.PING
_PONG = bytes([0x02])            # MsgType.PONG
_SHUTDOWN_CLIENT = bytes([0x05]) # MsgType.SHUTDOWN_CLIENT


_IPC_SOCK_DIR = '/tmp/c_two_ipc'


def _resolve_socket_path(region_id: str) -> str:
    return os.path.join(_IPC_SOCK_DIR, f'{region_id}.sock')


def _encode_frame(request_id: int, flags: int, payload: bytes) -> bytes:
    total = 12 + len(payload)
    buf = bytearray(16 + len(payload))
    _FRAME_STRUCT.pack_into(buf, 0, total, request_id, flags)
    buf[16:] = payload
    return bytes(buf)


def _recv_exact(sock: _socket.socket, n: int) -> bytes:
    if n == 0:
        return b''
    parts: list[bytes] = []
    remaining = n
    while remaining > 0:
        chunk = sock.recv(remaining)
        if not chunk:
            raise ConnectionError('Connection closed')
        parts.append(chunk)
        remaining -= len(chunk)
    return b''.join(parts)


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
        frame = _encode_frame(0, _FLAG_SIGNAL, _PING)
        sock.sendall(frame)
        header = _recv_exact(sock, 16)
        total_len, _rid, _flags = _FRAME_STRUCT.unpack(header)
        payload_len = total_len - 12
        payload = _recv_exact(sock, payload_len) if payload_len > 0 else b''
        return len(payload) == 1 and payload[0] == _PONG[0]
    except Exception:
        return False
    finally:
        sock.close()


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
        frame = _encode_frame(0, _FLAG_SIGNAL, _SHUTDOWN_CLIENT)
        sock.sendall(frame)
        header = _recv_exact(sock, 16)
        total_len, _rid, _flags = _FRAME_STRUCT.unpack(header)
        payload_len = total_len - 12
        if payload_len > 0:
            _recv_exact(sock, payload_len)
        return True
    except Exception:
        return False
    finally:
        sock.close()
