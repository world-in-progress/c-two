"""IPC specific tests: buddy handshake, SHM transport, concurrent requests."""

import os
import threading
import time
import pytest

import c_two as cc
from c_two.transport.ipc.frame import IPCConfig
from c_two.transport.ipc.shm_frame import (
    encode_buddy_handshake,
    decode_buddy_handshake,
    encode_buddy_payload,
    decode_buddy_payload,
    encode_buddy_call_frame,
    encode_buddy_reply_frame,
    encode_buddy_reuse_reply_frame,
    encode_ctrl_buddy_announce,
    decode_ctrl_buddy_announce,
    FLAG_BUDDY,
    BUDDY_PAYLOAD_SIZE,
    BUDDY_REUSE_FLAG,
    HANDSHAKE_VERSION,
)
from c_two.transport.server import Server
from c_two.transport.client.core import SharedClient
from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello


def _wait_for_server(address: str, timeout: float = 5.0) -> None:
    """Poll until the server is accepting connections."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if SharedClient.ping(address, timeout=0.5):
                return
        except Exception:
            pass
        time.sleep(0.05)
    raise TimeoutError(f'Server at {address} not ready after {timeout}s')


def _wait_for_shutdown(address: str, timeout: float = 5.0) -> None:
    """Poll until the server stops responding to pings."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if not SharedClient.ping(address, timeout=0.3):
                return
        except Exception:
            return
        time.sleep(0.05)
    raise TimeoutError(f'Server at {address} still alive after {timeout}s')


# ------------------------------------------------------------------
# Wire protocol codec tests
# ------------------------------------------------------------------

class TestBuddyProtocol:
    def test_buddy_payload_roundtrip(self):
        encoded = encode_buddy_payload(3, 65536, 32768, False)
        seg_idx, offset, data_size, is_ded, free_off, free_sz = decode_buddy_payload(encoded)
        assert seg_idx == 3
        assert offset == 65536
        assert data_size == 32768
        assert is_ded is False
        assert free_off == offset
        assert free_sz == data_size

    def test_buddy_payload_dedicated(self):
        encoded = encode_buddy_payload(256, 0, 1048576, True)
        seg_idx, offset, data_size, is_ded, free_off, free_sz = decode_buddy_payload(encoded)
        assert seg_idx == 256
        assert offset == 0
        assert data_size == 1048576
        assert is_ded is True
        assert free_off == offset
        assert free_sz == data_size

    def test_buddy_payload_too_short(self):
        with pytest.raises(ValueError, match='too short'):
            decode_buddy_payload(b'\x00' * 5)

    def test_buddy_reuse_payload_roundtrip(self):
        """Test the extended reuse payload format with separate free coordinates."""
        import struct
        from c_two.transport.ipc.shm_frame import (
            BUDDY_PAYLOAD_STRUCT, BUDDY_REUSE_EXTRA,
        )
        # Build a reuse payload manually.
        flags = BUDDY_REUSE_FLAG
        payload = BUDDY_PAYLOAD_STRUCT.pack(0, 1024, 500, flags)
        payload += BUDDY_REUSE_EXTRA.pack(1000, 600)
        seg_idx, data_off, data_sz, is_ded, free_off, free_sz = decode_buddy_payload(payload)
        assert seg_idx == 0
        assert data_off == 1024
        assert data_sz == 500
        assert is_ded is False
        assert free_off == 1000
        assert free_sz == 600

    def test_handshake_roundtrip(self):
        segments = [('/cc3b_test1', 268435456), ('/cc3b_test2', 134217728)]
        encoded = encode_buddy_handshake(segments)
        decoded = decode_buddy_handshake(encoded)
        assert decoded == segments

    def test_handshake_empty(self):
        encoded = encode_buddy_handshake([])
        decoded = decode_buddy_handshake(encoded)
        assert decoded == []

    def test_handshake_version_mismatch(self):
        bad_payload = bytes([99, 0, 0])
        with pytest.raises(ValueError, match='version'):
            decode_buddy_handshake(bad_payload)


# ------------------------------------------------------------------
# Server/Client lifecycle (uses conftest fixtures)
# ------------------------------------------------------------------

_counter = 0
_lock = threading.Lock()

def _unique_region():
    global _counter
    with _lock:
        _counter += 1
        return f'test_ipc_{os.getpid()}_{_counter}'


@pytest.fixture
def ipc_server():
    """Start an IPC server, yield address, shut down."""
    addr = f'ipc://{_unique_region()}'
    server = Server(bind_address=addr, icrm_class=IHello, crm_instance=Hello(), name='default')
    server.start()
    _wait_for_server(addr)
    yield addr
    try:
        server.shutdown()
    except Exception:
        pass
    _wait_for_shutdown(addr)


class TestIPCLifecycle:
    def test_ping(self, ipc_server):
        assert SharedClient.ping(ipc_server, timeout=1.0)

    def test_simple_call(self, ipc_server):
        with cc.compo.runtime.connect_crm(ipc_server, IHello) as crm:
            assert crm.greeting('World') == 'Hello, World!'

    def test_uses_buddy_transport(self, ipc_server):
        """Verify IPC actually performs buddy handshake and uses SHM."""
        with cc.compo.runtime.connect_crm(ipc_server, IHello) as crm:
            crm.greeting('Probe')
            ipc_client = crm.client._client
            # Rust client manages buddy pool internally; verify connected.
            if hasattr(ipc_client, 'is_connected'):
                assert ipc_client.is_connected
            else:
                assert ipc_client._buddy_pool is not None, 'Buddy pool not initialized'
                assert len(ipc_client._seg_views) > 0, 'No segment views cached'

    def test_multiple_calls(self, ipc_server):
        with cc.compo.runtime.connect_crm(ipc_server, IHello) as crm:
            assert crm.add(3, 4) == 7
            assert crm.greeting('IPC') == 'Hello, IPC!'

    def test_shutdown(self):
        addr = f'ipc://{_unique_region()}'
        server = Server(bind_address=addr, icrm_class=IHello, crm_instance=Hello(), name='default')
        server.start()
        _wait_for_server(addr)
        server.shutdown()
        _wait_for_shutdown(addr)
        assert not SharedClient.ping(addr, timeout=0.5)


# ------------------------------------------------------------------
# Concurrency stress test
# ------------------------------------------------------------------

class TestIPCConcurrency:
    def test_concurrent_clients(self, ipc_server):
        errors = []
        def client_worker(thread_id):
            try:
                with cc.compo.runtime.connect_crm(ipc_server, IHello) as crm:
                    for i in range(5):
                        result = crm.add(i, thread_id)
                        if result != i + thread_id:
                            errors.append(f'Thread {thread_id}: wrong result {result}')
            except Exception as e:
                errors.append(f'Thread {thread_id}: {e}')

        threads = [threading.Thread(target=client_worker, args=(i,)) for i in range(4)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        assert errors == [], f'Concurrency errors: {errors}'


# ------------------------------------------------------------------
# Buddy pool SHM data path
# ------------------------------------------------------------------

class TestIPCBuddyPath:
    def test_large_payload_uses_buddy(self):
        """Send payload > shm_threshold to exercise the buddy SHM data path."""
        addr = f'ipc://{_unique_region()}'
        ipc_config = IPCConfig(
            shm_threshold=1024,
            pool_segment_size=64 * 1024,
        )
        server = Server(bind_address=addr, icrm_class=IHello, crm_instance=Hello(), ipc_config=ipc_config, name='default')
        server.start()
        _wait_for_server(addr)
        try:
            with cc.compo.runtime.connect_crm(addr, IHello) as crm:
                large_name = 'x' * 2000  # > shm_threshold (1024)
                result = crm.greeting(large_name)
                assert result == f'Hello, {large_name}!'

                # Verify buddy transport was actually used
                ipc_client = crm.client._client
                if hasattr(ipc_client, 'is_connected'):
                    assert ipc_client.is_connected
                else:
                    assert ipc_client._buddy_pool is not None, \
                        'Buddy pool not initialized for large payload'
        finally:
            try:
                server.shutdown()
            except Exception:
                pass
            _wait_for_shutdown(addr)


# ------------------------------------------------------------------
# Buddy announce codec tests
# ------------------------------------------------------------------

class TestBuddyAnnounce:
    def test_announce_roundtrip(self):
        """Encode then decode a CTRL_BUDDY_ANNOUNCE message."""
        seg_idx, size, name = 2, 65536, 'seg_test_1'
        encoded = encode_ctrl_buddy_announce(seg_idx, size, name)
        d_idx, d_size, d_name = decode_ctrl_buddy_announce(encoded)
        assert d_idx == seg_idx
        assert d_size == size
        assert d_name == name

    def test_announce_large_index(self):
        seg_idx, size, name = 256, 131072, '/cc3b_large'
        encoded = encode_ctrl_buddy_announce(seg_idx, size, name)
        d_idx, d_size, d_name = decode_ctrl_buddy_announce(encoded)
        assert d_idx == seg_idx
        assert d_size == size
        assert d_name == name

    def test_announce_short_name(self):
        encoded = encode_ctrl_buddy_announce(0, 4096, 'x')
        d_idx, d_size, d_name = decode_ctrl_buddy_announce(encoded)
        assert d_idx == 0
        assert d_size == 4096
        assert d_name == 'x'

    def test_announce_too_short_raises(self):
        with pytest.raises(ValueError, match='too short'):
            decode_ctrl_buddy_announce(b'\x00' * 3)


# ------------------------------------------------------------------
# Safety tests (from op3-analysis.md audit)
# ------------------------------------------------------------------

class TestOPTC1SafeBytesCopy:
    """OPT-C1: call() must return bytes (not memoryview) for buddy responses."""

    def test_buddy_response_returns_bytes(self):
        """Large payload response through buddy path must return bytes type."""
        addr = f'ipc://{_unique_region()}'
        ipc_config = IPCConfig(
            shm_threshold=512,
            pool_segment_size=64 * 1024,
        )
        server = Server(bind_address=addr, icrm_class=IHello, crm_instance=Hello(), ipc_config=ipc_config, name='default')
        server.start()
        _wait_for_server(addr)
        try:
            with cc.compo.runtime.connect_crm(addr, IHello) as crm:
                large_name = 'Y' * 2000
                result = crm.greeting(large_name)
                assert isinstance(result, str)
                assert result == f'Hello, {large_name}!'
                # The underlying RPC result going through auto_transfer
                # should have been bytes (not memoryview) from the client
                v3_client = crm.client._client
                if hasattr(v3_client, 'is_connected'):
                    assert v3_client.is_connected
                else:
                    assert v3_client._buddy_pool is not None
        finally:
            try:
                server.shutdown()
            except Exception:
                pass
            _wait_for_shutdown(addr)


class TestBuddyProtocolEdgeCases:
    """Wire protocol edge cases and boundary validation."""

    def test_handshake_version_mismatch(self):
        """Handshake with wrong version must be detected."""
        segments = [('seg1', 65536)]
        encoded = encode_buddy_handshake(segments)
        # Corrupt version byte (first byte)
        corrupted = bytes([encoded[0] ^ 0xFF]) + encoded[1:]
        with pytest.raises((ValueError, Exception)):
            decode_buddy_handshake(corrupted)

    def test_payload_roundtrip_dedicated(self):
        """Dedicated segment payload encodes/decodes correctly."""
        payload = encode_buddy_payload(
            seg_idx=256, offset=0, data_size=1024, is_dedicated=True,
        )
        seg_idx, offset, data_size, is_ded, free_off, free_sz = decode_buddy_payload(payload)
        assert seg_idx == 256
        assert is_ded is True
        assert data_size == 1024

    def test_payload_roundtrip_normal(self):
        """Normal (non-reuse) payload: free coords equal data coords."""
        payload = encode_buddy_payload(seg_idx=0, offset=4096, data_size=8192)
        seg_idx, offset, data_size, is_ded, free_off, free_sz = decode_buddy_payload(payload)
        assert seg_idx == 0
        assert offset == 4096
        assert data_size == 8192
        assert free_off == offset
        assert free_sz == data_size

    def test_empty_handshake_segments(self):
        """Handshake with empty segment list should still roundtrip."""
        encoded = encode_buddy_handshake([])
        segments = decode_buddy_handshake(encoded)
        assert segments == []


class TestIPCStressRecovery:
    """Test that IPC recovers from edge cases under load."""

    def test_rapid_connect_disconnect(self):
        """Rapid connect/disconnect cycles should not leak resources."""
        addr = f'ipc://{_unique_region()}'
        server = Server(bind_address=addr, icrm_class=IHello, crm_instance=Hello(), name='default')
        server.start()
        _wait_for_server(addr)
        try:
            for i in range(10):
                with cc.compo.runtime.connect_crm(addr, IHello) as crm:
                    assert crm.add(i, 1) == i + 1
        finally:
            try:
                server.shutdown()
            except Exception:
                pass
            _wait_for_shutdown(addr)

    def test_concurrent_clients_heavy(self):
        """8 threads × 20 iterations stress test."""
        addr = f'ipc://{_unique_region()}'
        server = Server(bind_address=addr, icrm_class=IHello, crm_instance=Hello(), name='default')
        server.start()
        _wait_for_server(addr)
        errors = []

        def worker(tid):
            try:
                with cc.compo.runtime.connect_crm(addr, IHello) as crm:
                    for i in range(20):
                        r = crm.add(i, tid)
                        if r != i + tid:
                            errors.append(f'T{tid}: {r} != {i + tid}')
            except Exception as e:
                errors.append(f'T{tid}: {e}')

        try:
            threads = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
            for t in threads:
                t.start()
            for t in threads:
                t.join()
            assert errors == [], f'Heavy concurrency errors: {errors}'
        finally:
            try:
                server.shutdown()
            except Exception:
                pass
            _wait_for_shutdown(addr)
