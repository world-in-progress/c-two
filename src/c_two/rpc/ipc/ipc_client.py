"""
IPC v2 Client — connects to IPCv2Server via UDS control plane.

Uses SharedMemory data plane for large payloads with ownership transfer:
client creates SHM → sends reference → server takes ownership and releases.
"""

import asyncio
import logging
import os
import struct
import tempfile
import threading
import time
import uuid
from multiprocessing import shared_memory

from ... import error
from ..base import BaseClient
from ..event import Event, EventTag
from ..util.encoding import add_length_prefix, parse_message
from .ipc_server import (
    DEFAULT_INLINE_THRESHOLD,
    DEFAULT_SHM_THRESHOLD,
    IPCConfig,
    _FLAG_RESPONSE,
    _FLAG_SHM,
    _decode_frame,
    _encode_frame,
    _read_and_release_shm,
    _read_frame,
    _shm_name,
    _write_frame,
    _write_shm,
)

logger = logging.getLogger(__name__)


def _resolve_socket_path(region_id: str) -> str:
    tmpdir = os.getenv('IPC_V2_SOCKET_DIR', tempfile.gettempdir())
    return os.path.join(tmpdir, f'cc_ipcv2_{region_id}.sock')


class IPCv2Client(BaseClient):

    def __init__(self, server_address: str, ipc_config: IPCConfig | None = None):
        super().__init__(server_address)
        self._config = ipc_config or IPCConfig()
        self.region_id = server_address.replace('ipc-v2://', '')
        self._socket_path = _resolve_socket_path(self.region_id)
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None
        self._loop: asyncio.AbstractEventLoop | None = None
        self._conn_lock = threading.Lock()

    def _ensure_loop(self) -> asyncio.AbstractEventLoop:
        if self._loop is None or self._loop.is_closed():
            self._loop = asyncio.new_event_loop()
        return self._loop

    def _connect_sync(self) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
        loop = self._ensure_loop()
        reader, writer = loop.run_until_complete(
            asyncio.open_unix_connection(path=self._socket_path)
        )
        return reader, writer

    def _send_and_recv(self, request_id: str, flags: int, payload: bytes) -> tuple[str, int, bytes]:
        with self._conn_lock:
            loop = self._ensure_loop()
            reader, writer = loop.run_until_complete(
                asyncio.open_unix_connection(path=self._socket_path)
            )
            try:
                frame = _encode_frame(request_id, flags, payload)
                loop.run_until_complete(_write_frame(writer, frame))
                raw = loop.run_until_complete(_read_frame(reader))
                return _decode_frame(raw)
            finally:
                writer.close()
                try:
                    loop.run_until_complete(writer.wait_closed())
                except Exception:
                    pass

    def _create_method_event(self, method_name: str, data: bytes | None = None) -> Event:
        try:
            request_id = str(uuid.uuid4())
            serialized_data = b'' if data is None else data
            serialized_method_name = method_name.encode('utf-8')
            combined_request = add_length_prefix(serialized_method_name) + add_length_prefix(serialized_data)
            return Event(tag=EventTag.CRM_CALL, data=combined_request, request_id=request_id)
        except Exception as e:
            raise error.CompoSerializeInput(f'Error occurred when serializing request: {e}')

    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        event = self._create_method_event(method_name, data)
        event_bytes = event.serialize()
        request_id = event.request_id
        flags = 0

        # Decide inline vs SHM
        if len(event_bytes) >= self._config.shm_threshold:
            shm_name = _shm_name(self.region_id, request_id, 'req')
            shm = _write_shm(shm_name, event_bytes)
            shm.close()  # ownership transfers to server
            size_header = struct.pack('<Q', len(event_bytes))
            payload = shm_name.encode('utf-8') + b'\x00' + size_header
            flags |= _FLAG_SHM
        else:
            payload = event_bytes

        try:
            resp_rid, resp_flags, resp_payload = self._send_and_recv(request_id, flags, payload)
        except Exception as exc:
            raise error.CompoClientError(f'IPC v2 call failed: {exc}') from exc

        # Decode response
        if resp_flags & _FLAG_SHM:
            parts = resp_payload.split(b'\x00', 1)
            shm_name = parts[0].decode('utf-8')
            size = struct.unpack('<Q', parts[1])[0]
            response_bytes = _read_and_release_shm(shm_name, size)
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
            shm_name = parts[0].decode('utf-8')
            size = struct.unpack('<Q', parts[1])[0]
            return _read_and_release_shm(shm_name, size)

        return resp_payload

    def terminate(self) -> None:
        if self._loop is not None and not self._loop.is_closed():
            self._loop.close()
            self._loop = None

    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        region_id = server_address.replace('ipc-v2://', '')
        socket_path = _resolve_socket_path(region_id)

        if not os.path.exists(socket_path):
            return False

        loop = asyncio.new_event_loop()
        try:
            reader, writer = loop.run_until_complete(
                asyncio.wait_for(
                    asyncio.open_unix_connection(path=socket_path),
                    timeout=timeout,
                )
            )

            request_id = str(uuid.uuid4())
            ping_event = Event(tag=EventTag.PING, request_id=request_id)
            event_bytes = ping_event.serialize()
            frame = _encode_frame(request_id, 0, event_bytes)

            loop.run_until_complete(_write_frame(writer, frame))
            raw = loop.run_until_complete(
                asyncio.wait_for(_read_frame(reader), timeout=timeout)
            )
            resp_rid, resp_flags, resp_payload = _decode_frame(raw)
            resp_event = Event.deserialize(resp_payload)

            writer.close()
            try:
                loop.run_until_complete(writer.wait_closed())
            except Exception:
                pass

            return resp_event.tag == EventTag.PONG
        except Exception:
            return False
        finally:
            loop.close()

    @staticmethod
    def shutdown(server_address: str, timeout: float = 0.5) -> bool:
        region_id = server_address.replace('ipc-v2://', '')
        socket_path = _resolve_socket_path(region_id)

        if not os.path.exists(socket_path):
            return True

        loop = asyncio.new_event_loop()
        try:
            reader, writer = loop.run_until_complete(
                asyncio.wait_for(
                    asyncio.open_unix_connection(path=socket_path),
                    timeout=timeout,
                )
            )

            request_id = str(uuid.uuid4())
            shutdown_event = Event(tag=EventTag.SHUTDOWN_FROM_CLIENT, request_id=request_id)
            event_bytes = shutdown_event.serialize()
            frame = _encode_frame(request_id, 0, event_bytes)

            loop.run_until_complete(_write_frame(writer, frame))
            raw = loop.run_until_complete(
                asyncio.wait_for(_read_frame(reader), timeout=timeout)
            )
            resp_rid, resp_flags, resp_payload = _decode_frame(raw)
            resp_event = Event.deserialize(resp_payload)

            writer.close()
            try:
                loop.run_until_complete(writer.wait_closed())
            except Exception:
                pass

            return resp_event.tag == EventTag.SHUTDOWN_ACK
        except Exception:
            return False
        finally:
            loop.close()
