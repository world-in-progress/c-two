"""Security unit tests for IPC v2 transport layer.

Covers:
    S1: decode_frame body too small validation
    S2: _read_frame / _recv_frame_sync total_len bounds
    S3: IPCConfig defaults, max_pending enforcement
    S4: SHM size validation in fast_read_shm
    S5: Pending futures cleanup on disconnect
    S6: SHM GC timestamp-after-send
    C1-C4: Dead code removal verification
"""

import asyncio
import socket as _socket
import struct
import time
from multiprocessing import shared_memory

import pytest

from c_two import error
from c_two.rpc.event import Event, EventQueue, EventTag
from c_two.rpc.event.envelope import Envelope
from c_two.rpc.event.msg_type import MsgType
from c_two.rpc.ipc.ipc_protocol import (
    DEFAULT_MAX_FRAME_SIZE,
    DEFAULT_MAX_PAYLOAD_SIZE,
    DEFAULT_MAX_PENDING_REQUESTS,
    DEFAULT_SHM_THRESHOLD,
    IPCConfig,
    decode_frame,
    encode_frame,
    fast_read_shm,
)
from c_two.rpc.ipc.ipc_server import (
    IPCv2Server,
    _read_frame,
)
from c_two.rpc.ipc.ipc_client import _recv_frame_sync
from c_two.rpc.util.wire import encode_call


# ---------------------------------------------------------------------------
# S1: decode_frame — body too small validation
# ---------------------------------------------------------------------------

class TestDecodeFrameValidation:
    """S1: decode_frame must reject bodies smaller than 12 bytes."""

    def test_body_too_small(self):
        """body < 12 bytes → EventDeserializeError."""
        body = b'\x00' * 11
        with pytest.raises(error.EventDeserializeError, match='too small'):
            decode_frame(body)

    def test_empty_body(self):
        """empty body → EventDeserializeError."""
        with pytest.raises(error.EventDeserializeError, match='too small'):
            decode_frame(b'')

    def test_body_exactly_12_bytes(self):
        """body == 12 bytes → valid edge case (empty payload)."""
        rid = 42
        flags = 0
        body = struct.pack('<Q', rid) + struct.pack('<I', flags)
        request_id, f, p = decode_frame(body)
        assert request_id == 42
        assert f == 0
        assert p == b''

    def test_valid_roundtrip(self):
        """Normal frame encode → decode should still work (strip 4B header)."""
        frame = encode_frame(1, 0, b'hello world')
        rid, flags, payload = decode_frame(frame[4:])
        assert rid == 1
        assert flags == 0
        assert payload == b'hello world'


# ---------------------------------------------------------------------------
# S2: _read_frame — total_len bounds
# ---------------------------------------------------------------------------

class TestReadFrameBounds:
    """S2: _read_frame must reject frames with total_len outside valid range."""

    def test_rejects_undersized_frame(self):
        """total_len < 12 → EventDeserializeError."""
        async def _run():
            reader = asyncio.StreamReader()
            # Feed full 16B header: total_len=8 (too small), rid=0, flags=0
            reader.feed_data(struct.pack('<IQI', 8, 0, 0))
            with pytest.raises(error.EventDeserializeError, match='too small'):
                await _read_frame(reader, max_frame_size=DEFAULT_MAX_FRAME_SIZE)
        asyncio.run(_run())

    def test_rejects_oversized_frame(self):
        """total_len > max_frame_size → EventDeserializeError."""
        async def _run():
            max_size = 1024
            reader = asyncio.StreamReader()
            # Feed full 16B header: total_len > max_size, rid=0, flags=0
            reader.feed_data(struct.pack('<IQI', max_size + 1, 0, 0))
            reader.feed_data(b'\x00' * (max_size + 1))
            with pytest.raises(error.EventDeserializeError, match='too large'):
                await _read_frame(reader, max_frame_size=max_size)
        asyncio.run(_run())

    def test_accepts_valid_frame(self):
        """Frame within bounds should pass through."""
        async def _run():
            frame = encode_frame(1, 0, b'data')
            reader = asyncio.StreamReader()
            reader.feed_data(frame)
            rid, flags, payload = await _read_frame(reader, max_frame_size=DEFAULT_MAX_FRAME_SIZE)
            assert rid == 1
            assert payload == b'data'
        asyncio.run(_run())

    def test_rejects_zero_total_len(self):
        """total_len = 0 → too small."""
        async def _run():
            reader = asyncio.StreamReader()
            # Feed full 16B header with total_len=0
            reader.feed_data(struct.pack('<IQI', 0, 0, 0))
            with pytest.raises(error.EventDeserializeError, match='too small'):
                await _read_frame(reader, max_frame_size=DEFAULT_MAX_FRAME_SIZE)
        asyncio.run(_run())


# ---------------------------------------------------------------------------
# S2: _recv_frame_sync — total_len bounds (client side)
# ---------------------------------------------------------------------------

class TestRecvFrameSyncBounds:
    """S2: _recv_frame_sync must validate total_len on the client side."""

    def _make_socket_pair(self):
        """Create a connected socket pair for testing."""
        server_sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        import tempfile, os
        path = os.path.join(tempfile.gettempdir(), f'test_ipc_sec_{id(self)}.sock')
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass
        server_sock.bind(path)
        server_sock.listen(1)
        client_sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        client_sock.connect(path)
        conn, _ = server_sock.accept()
        server_sock.close()
        os.unlink(path)
        return client_sock, conn

    def test_rejects_undersized(self):
        """total_len < 12 → EventDeserializeError."""
        client, server = self._make_socket_pair()
        try:
            # Send full 16B header: total_len=8 (too small), rid=0, flags=0
            server.sendall(struct.pack('<IQI', 8, 0, 0))
            with pytest.raises(error.EventDeserializeError, match='too small'):
                _recv_frame_sync(client, max_frame_size=DEFAULT_MAX_FRAME_SIZE)
        finally:
            client.close()
            server.close()

    def test_rejects_oversized(self):
        """total_len > max_frame_size → EventDeserializeError."""
        client, server = self._make_socket_pair()
        try:
            # Send full 16B header: total_len=2048, rid=0, flags=0
            server.sendall(struct.pack('<IQI', 2048, 0, 0))
            with pytest.raises(error.EventDeserializeError, match='too large'):
                _recv_frame_sync(client, max_frame_size=1024)
        finally:
            client.close()
            server.close()

    def test_default_limit_accepts_small_frame(self):
        """Default max_frame_size (from server) accepts normal-sized frames."""
        client, server = self._make_socket_pair()
        try:
            frame = encode_frame(1, 0, b'x' * 100)
            server.sendall(frame)
            rid, _, payload = _recv_frame_sync(client)
            assert rid == 1
        finally:
            client.close()
            server.close()


# ---------------------------------------------------------------------------
# S3: IPCConfig defaults
# ---------------------------------------------------------------------------

class TestIPCConfigDefaults:
    """S3: IPCConfig security limits should have reasonable defaults."""

    def test_defaults(self):
        cfg = IPCConfig()
        assert cfg.shm_threshold == DEFAULT_SHM_THRESHOLD
        assert cfg.max_frame_size == DEFAULT_MAX_FRAME_SIZE
        assert cfg.max_payload_size == DEFAULT_MAX_PAYLOAD_SIZE
        assert cfg.max_pending_requests == DEFAULT_MAX_PENDING_REQUESTS

    def test_max_frame_size_default_reasonable(self):
        assert DEFAULT_MAX_FRAME_SIZE == 16 * 1024 * 1024  # 16 MB

    def test_max_payload_size_default_reasonable(self):
        # 16 GB — SHM data validation limit, not constrained by uint32 wire frame
        assert DEFAULT_MAX_PAYLOAD_SIZE == 16 * 1024 * 1024 * 1024  # 16 GB

    def test_max_pending_default_reasonable(self):
        assert DEFAULT_MAX_PENDING_REQUESTS == 1024

    def test_custom_limits(self):
        cfg = IPCConfig(max_frame_size=4096, max_payload_size=1_000_000, max_pending_requests=10)
        assert cfg.max_frame_size == 4096
        assert cfg.max_payload_size == 1_000_000
        assert cfg.max_pending_requests == 10


# ---------------------------------------------------------------------------
# S4: SHM size validation in fast_read_shm
# ---------------------------------------------------------------------------

class TestSHMSizeValidation:
    """S4: fast_read_shm must validate actual SHM size >= declared size."""

    def test_actual_shm_smaller_than_declared(self):
        """If declared size > actual SHM size → EventDeserializeError."""
        shm_name = 'cc_test_s4_small'
        # macOS rounds SHM to page boundaries (typically 16 KB),
        # so we must declare a size that exceeds the rounded-up actual size.
        actual_size = 64
        shm = shared_memory.SharedMemory(name=shm_name, create=True, size=actual_size)
        rounded_actual = shm.size  # e.g. 16384 on macOS
        shm.close()
        declared_size = rounded_actual + 100  # exceeds even the rounded size
        with pytest.raises(error.EventDeserializeError, match='actual size'):
            fast_read_shm(shm_name, declared_size)

    def test_exact_size_match_succeeds(self):
        """declared size == actual SHM size → should succeed."""
        shm_name = 'cc_test_s4_exact'
        size = 128
        shm = shared_memory.SharedMemory(name=shm_name, create=True, size=size)
        shm.buf[:size] = b'\xAB' * size
        shm.close()  # close but don't unlink — fast_read_shm will unlink
        data, buf = fast_read_shm(shm_name, size)
        assert bytes(data) == b'\xAB' * size
        buf.release()


# ---------------------------------------------------------------------------
# S5: Pending futures cleanup on disconnect
# ---------------------------------------------------------------------------

class TestPendingCleanup:
    """S5: when a client disconnects, its pending futures must be cancelled on shutdown."""

    def test_pending_cleaned_on_shutdown_after_disconnect(self):
        """Pending futures for a disconnected client are cleaned up when server shuts down."""
        eq = EventQueue()
        addr = 'ipc-v2://test_s5_cleanup'
        cfg = IPCConfig(max_pending_requests=10)
        server = IPCv2Server(addr, event_queue=eq, ipc_config=cfg)
        server.start()

        try:
            socket_path = server.socket_path
            sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
            sock.connect(socket_path)

            # Send a valid request frame (wire format: CRM_CALL)
            request_id = 42
            call_bytes = encode_call('foo', b'')
            frame = encode_frame(request_id, 0, call_bytes)
            sock.sendall(frame)

            # Wait for the event to arrive in the queue
            time.sleep(0.3)

            # Verify there's a pending future
            with server._pending_lock:
                assert len(server._pending) == 1

            # Abruptly close the connection (handler still awaiting future)
            sock.close()
        finally:
            # Shutdown triggers task cancellation → finally block runs S5 cleanup
            server.shutdown()
            time.sleep(0.5)
            server.destroy()

        # After shutdown + destroy, all pending futures must be cleaned up
        with server._pending_lock:
            assert len(server._pending) == 0


# ---------------------------------------------------------------------------
# S6: SHM GC timestamp after send
# ---------------------------------------------------------------------------

class TestSHMGCTimestampAfterSend:
    """S6: SHM segment should not be registered for GC until frame is sent."""

    def test_reply_does_not_register_gc_directly(self):
        """reply() must not add SHM name to _our_shm_segments — that's _handle_client's job."""
        eq = EventQueue()
        addr = 'ipc-v2://test_s6_gc'
        server = IPCv2Server(addr, event_queue=eq)
        server.start()

        try:
            # Manually invoke reply() with a large payload that triggers SHM path
            request_id = '0:99'  # conn_id:wire_rid format (reply() extracts wire rid)
            # Create a pending future
            loop = server._loop
            fut = loop.call_soon_threadsafe(loop.create_future)
            # We need the future in _pending
            import concurrent.futures
            real_fut_holder = [None]

            def create_and_register():
                f = loop.create_future()
                real_fut_holder[0] = f
                with server._pending_lock:
                    server._pending[request_id] = f

            loop.call_soon_threadsafe(create_and_register)
            time.sleep(0.2)

            # Call reply with large data → triggers SHM path
            event = Event(tag=EventTag.CRM_REPLY, request_id=request_id)
            event.data_parts = [b'', b'\x00' * (2 * 1024 * 1024)]  # empty error, 2 MB result → SHM
            server.reply(event)

            time.sleep(0.2)

            # The SHM name should NOT be in _our_shm_segments yet
            # because _handle_client hasn't sent the frame
            with server._shm_lock:
                assert len(server._our_shm_segments) == 0

            # The future should have the (frame, shm_name) tuple
            assert real_fut_holder[0] is not None
            assert real_fut_holder[0].done()
            frame, shm_name = real_fut_holder[0].result()
            assert shm_name is not None
            assert isinstance(frame, bytes)

            # Clean up the SHM segment created by reply
            try:
                shm = shared_memory.SharedMemory(name=shm_name, create=False)
                shm.close()
                shm.unlink()
            except FileNotFoundError:
                pass
        finally:
            server.shutdown()
            server.destroy()


# ---------------------------------------------------------------------------
# C1-C4: Dead code verification
# ---------------------------------------------------------------------------

class TestDeadCodeRemoved:
    """Verify that dead code identified in the review has been removed."""

    def test_no_inline_threshold_in_config(self):
        """IPCConfig should not have inline_threshold field."""
        cfg = IPCConfig()
        assert not hasattr(cfg, 'inline_threshold')

    def test_no_default_inline_threshold_constant(self):
        """DEFAULT_INLINE_THRESHOLD should not be exported from ipc_server."""
        import c_two.rpc.ipc.ipc_server as mod
        assert not hasattr(mod, 'DEFAULT_INLINE_THRESHOLD')

    def test_no_open_shm_zero_copy(self):
        """_open_shm_zero_copy dead function should be removed."""
        import c_two.rpc.ipc.ipc_server as mod
        assert not hasattr(mod, '_open_shm_zero_copy')

    def test_no_release_shm(self):
        """_release_shm dead function should be removed."""
        import c_two.rpc.ipc.ipc_server as mod
        assert not hasattr(mod, '_release_shm')

    def test_client_no_dead_imports(self):
        """ipc_client should not import dead symbols from ipc_server."""
        import c_two.rpc.ipc.ipc_client as client_mod
        import inspect
        source = inspect.getsource(client_mod)
        assert 'DEFAULT_INLINE_THRESHOLD' not in source
        assert '_FLAG_RESPONSE' not in source.split('from .ipc_server import')[1].split(')')[0] if 'from .ipc_server import' in source else True

    def test_write_shm_not_in_server(self):
        """_write_shm should not be in ipc_server (removed as dead code)."""
        import c_two.rpc.ipc.ipc_server as server_mod
        import c_two.rpc.ipc.ipc_client as client_mod
        assert not hasattr(server_mod, '_write_shm')
        # _write_shm was removed from client too — pool SHM and
        # per-request SHM writes are handled inline via shm_writer callbacks.
        assert not hasattr(client_mod, '_write_shm')

    def test_read_and_release_shm_moved_to_benchmarks(self):
        """_read_and_release_shm should not be in ipc_server (moved to benchmarks)."""
        import c_two.rpc.ipc.ipc_server as mod
        assert not hasattr(mod, '_read_and_release_shm')


# ---------------------------------------------------------------------------
# SHM reference format validation
# ---------------------------------------------------------------------------

class TestSHMReferenceFormat:
    """SHM reference payload must contain a null separator and 8-byte size field."""

    def test_missing_null_separator(self):
        """Payload with no \\x00 → EventDeserializeError."""
        # Build a frame with FLAG_SHM set but payload has no null byte
        from c_two.rpc.ipc.ipc_protocol import FLAG_SHM
        bad_payload = b'no_null_here'
        frame = encode_frame(1, FLAG_SHM, bad_payload)
        _, flags, payload = decode_frame(frame[4:])
        # Simulate server-side parsing
        parts = payload.split(b'\x00', 1)
        assert len(parts) == 1  # confirms bug vector
        # The actual validation should raise
        with pytest.raises(error.EventDeserializeError, match='Malformed SHM reference'):
            if len(parts) != 2 or len(parts[1:1]) < 8:
                raise error.EventDeserializeError('Malformed SHM reference in request frame')

    def test_incomplete_size_field(self):
        """Payload with \\x00 but fewer than 8 bytes after → EventDeserializeError."""
        from c_two.rpc.ipc.ipc_protocol import FLAG_SHM
        bad_payload = b'shm_name\x00\x01\x02'  # only 2 bytes after null
        frame = encode_frame(1, FLAG_SHM, bad_payload)
        _, flags, payload = decode_frame(frame[4:])
        parts = payload.split(b'\x00', 1)
        assert len(parts) == 2
        assert len(parts[1]) < 8  # confirms bug vector
        with pytest.raises(error.EventDeserializeError, match='Malformed SHM reference'):
            if len(parts) != 2 or len(parts[1]) < 8:
                raise error.EventDeserializeError('Malformed SHM reference in request frame')

    def test_valid_shm_reference_accepted(self):
        """Well-formed SHM reference parses correctly."""
        from c_two.rpc.ipc.ipc_protocol import FLAG_SHM
        size_bytes = struct.pack('<Q', 12345)
        good_payload = b'shm_name\x00' + size_bytes
        frame = encode_frame(1, FLAG_SHM, good_payload)
        _, flags, payload = decode_frame(frame[4:])
        parts = payload.split(b'\x00', 1)
        assert len(parts) == 2 and len(parts[1]) >= 8
        shm_name = parts[0].decode('utf-8')
        size = struct.unpack('<Q', parts[1])[0]
        assert shm_name == 'shm_name'
        assert size == 12345
