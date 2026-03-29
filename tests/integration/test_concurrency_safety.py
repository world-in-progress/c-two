"""Concurrency safety and security integration tests for rpc_v2.

Tests:
- Thread-safe concurrent calls through SharedClient
- Request ID 32-bit wrapping correctness
- Server segment limit enforcement
- Double-terminate safety
- Concurrent connect/close lifecycle
"""
from __future__ import annotations

import os
import time
import threading
import struct

import pytest

import c_two as cc
from c_two.transport.client.core import SharedClient
from c_two.transport.server.core import ServerV2
from c_two.transport.client.pool import ClientPool
from c_two.transport.ipc.frame import IPCConfig


# ---------------------------------------------------------------------------
# Test fixtures
# ---------------------------------------------------------------------------

@cc.icrm(namespace='cc.test.concurrency', version='0.1.0')
class IEcho:
    def echo(self, data: bytes) -> bytes: ...
    def add(self, a: int, b: int) -> int: ...

class EchoImpl:
    def echo(self, data: bytes) -> bytes:
        return data
    def add(self, a: int, b: int) -> int:
        return a + b


_counter = 0

def _next_addr():
    global _counter
    _counter += 1
    return f'ipc-v3://test_conc_{os.getpid()}_{_counter}'


def _wait_for_server(addr: str, timeout: float = 3.0):
    """Poll until server socket is ready."""
    import socket as _sock
    sock_dir = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')
    region = addr.replace('ipc-v3://', '').replace('ipc://', '')
    sock_path = os.path.join(sock_dir, region + '.sock')
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.exists(sock_path):
            try:
                s = _sock.socket(_sock.AF_UNIX, _sock.SOCK_STREAM)
                s.connect(sock_path)
                s.close()
                return
            except Exception:
                pass
        time.sleep(0.02)
    raise TimeoutError(f'Server at {addr} not ready within {timeout}s')


# ---------------------------------------------------------------------------
# Request ID wrapping
# ---------------------------------------------------------------------------

class TestRequestIdWrapping:
    """Verify 32-bit request ID wrapping doesn't cause collisions."""

    def test_rid_wraps_at_32bit_boundary(self):
        """Counter wraps from 0xFFFFFFFF to 0."""
        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        try:
            client = SharedClient(addr, cfg)
            client.connect()

            # Simulate near-wrap: set counter close to 32-bit max.
            with client._rid_lock:
                client._rid_counter = 0xFFFFFFFE

            # Make 4 calls spanning the wrap point.
            results = []
            for i in range(4):
                r = client.call('echo', i.to_bytes(4, 'little'))
                results.append(r)

            # All 4 calls should succeed and return correct data.
            for i, r in enumerate(results):
                assert int.from_bytes(r, 'little') == i

            # Counter should have wrapped.
            with client._rid_lock:
                assert client._rid_counter == 2  # 0xFFFFFFFE + 4 wraps to 2

            client.terminate()
        finally:
            server.shutdown()


# ---------------------------------------------------------------------------
# Concurrent calls stress test
# ---------------------------------------------------------------------------

class TestConcurrentCallSafety:
    """Stress-test SharedClient concurrent call safety."""

    def test_32_threads_concurrent_echo(self):
        """32 threads making concurrent echo calls through one SharedClient."""
        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=1 << 20,  # 1MB
            max_pool_segments=2,
            max_pool_memory=2 << 20,
        )
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        try:
            client = SharedClient(addr, cfg)
            client.connect()

            errors = []
            results = {}

            def worker(thread_id: int, calls: int):
                try:
                    for i in range(calls):
                        tag = f'{thread_id}:{i}'.encode()
                        r = client.call('echo', tag)
                        assert r == tag, f'Thread {thread_id} call {i}: expected {tag!r}, got {r!r}'
                        results[(thread_id, i)] = True
                except Exception as e:
                    errors.append((thread_id, e))

            threads = []
            n_threads = 32
            calls_per_thread = 10
            for tid in range(n_threads):
                t = threading.Thread(target=worker, args=(tid, calls_per_thread))
                threads.append(t)
                t.start()
            for t in threads:
                t.join(timeout=15)

            assert not errors, f'Errors in threads: {errors}'
            assert len(results) == n_threads * calls_per_thread

            client.terminate()
        finally:
            server.shutdown()

    def test_concurrent_calls_different_sizes(self):
        """Concurrent calls with varied payload sizes (inline + buddy paths)."""
        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=1 << 20,
            max_pool_segments=2,
            max_pool_memory=2 << 20,
        )
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        try:
            client = SharedClient(addr, cfg)
            client.connect()

            errors = []
            sizes = [64, 256, 1024, 4096, 16384, 65536]

            def worker(thread_id: int, payload_size: int):
                try:
                    data = bytes(range(256)) * (payload_size // 256 + 1)
                    data = data[:payload_size]
                    for _ in range(5):
                        r = client.call('echo', data)
                        assert r == data
                except Exception as e:
                    errors.append((thread_id, payload_size, e))

            threads = []
            for tid, sz in enumerate(sizes * 2):  # 12 threads
                t = threading.Thread(target=worker, args=(tid, sz))
                threads.append(t)
                t.start()
            for t in threads:
                t.join(timeout=15)

            assert not errors, f'Errors: {errors}'
            client.terminate()
        finally:
            server.shutdown()


# ---------------------------------------------------------------------------
# Double-terminate safety
# ---------------------------------------------------------------------------

class TestDoubleTerminate:
    """Ensure calling terminate() multiple times is safe."""

    def test_client_double_terminate(self):
        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        try:
            client = SharedClient(addr, cfg)
            client.connect()
            r = client.call('echo', b'hello')
            assert r == b'hello'

            # Terminate twice — should not raise.
            client.terminate()
            client.terminate()
        finally:
            server.shutdown()

    def test_server_double_shutdown(self):
        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        # Shutdown twice — should not raise.
        server.shutdown()
        server.shutdown()


# ---------------------------------------------------------------------------
# ClientPool lifecycle safety
# ---------------------------------------------------------------------------

class TestClientPoolSafety:
    """Test ClientPool reference counting under concurrent access."""

    def test_concurrent_acquire_release(self):
        """Multiple threads acquiring/releasing the same address concurrently."""
        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        try:
            pool = ClientPool(grace_seconds=60.0, default_config=cfg)
            errors = []

            def worker(tid: int):
                try:
                    client = pool.acquire(addr)
                    r = client.call('echo', f'tid={tid}'.encode())
                    assert r == f'tid={tid}'.encode()
                    pool.release(addr)
                except Exception as e:
                    errors.append((tid, e))

            threads = []
            for tid in range(8):
                t = threading.Thread(target=worker, args=(tid,))
                threads.append(t)
                t.start()
            for t in threads:
                t.join(timeout=10)

            assert not errors, f'Errors: {errors}'

            # All released; refcount should be 0 but client still alive (grace period).
            assert pool.refcount(addr) == 0
            assert pool.has_client(addr)  # still in grace period

            pool.shutdown_all()
        finally:
            server.shutdown()

    def test_release_without_acquire_warns(self):
        """Releasing an address never acquired should not crash."""
        pool = ClientPool(grace_seconds=1.0)
        # Should not raise, just warn.
        pool.release('ipc-v3://nonexistent')
        pool.shutdown_all()


# ---------------------------------------------------------------------------
# Call after terminate raises cleanly
# ---------------------------------------------------------------------------

class TestCallAfterTerminate:
    """Verify calling a terminated client raises a clear error."""

    def test_call_after_terminate_raises(self):
        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        try:
            client = SharedClient(addr, cfg)
            client.connect()
            r = client.call('echo', b'ok')
            assert r == b'ok'

            client.terminate()

            with pytest.raises(Exception):
                client.call('echo', b'should fail')
        finally:
            server.shutdown()


# ---------------------------------------------------------------------------
# Terminate during in-flight calls
# ---------------------------------------------------------------------------

class TestTerminateDuringInFlight:
    """Verify terminate() wakes up in-flight callers cleanly."""

    def test_terminate_wakes_pending_callers(self):
        """Slow CRM + terminate → pending callers get error, not hang."""
        import time as _time

        @cc.icrm(namespace='cc.test.slow', version='0.1.0')
        class ISlow:
            def slow_op(self) -> bytes: ...

        class SlowImpl:
            def slow_op(self) -> bytes:
                _time.sleep(5.0)  # Simulate long operation
                return b'done'

        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(ISlow, SlowImpl())
        server.start()
        _wait_for_server(addr)

        try:
            client = SharedClient(addr, cfg)
            client.connect()

            errors = []

            def slow_caller():
                try:
                    client.call('slow_op', b'')
                except Exception as e:
                    errors.append(e)

            t = threading.Thread(target=slow_caller)
            t.start()

            # Give the call time to reach the server.
            _time.sleep(0.1)

            # Terminate while call is in-flight.
            client.terminate()

            # Caller thread should wake up quickly with an error.
            t.join(timeout=3.0)
            assert not t.is_alive(), 'Caller thread hung after terminate'
            assert len(errors) == 1
        finally:
            server.shutdown()


# ---------------------------------------------------------------------------
# Malformed frame handling — server stays alive
# ---------------------------------------------------------------------------

class TestServerMalformedFrames:
    """Verify server gracefully handles malformed frames without crashing."""

    def test_server_survives_malformed_v2_payload(self):
        """Send truncated v2 call control; server should disconnect client
        but keep serving others."""
        import socket as _sock

        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        try:
            sock_dir = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')
            region = addr.replace('ipc-v3://', '')
            sock_path = os.path.join(sock_dir, region + '.sock')

            # Send a malformed v2 call frame (truncated call control).
            raw = _sock.socket(_sock.AF_UNIX, _sock.SOCK_STREAM)
            raw.connect(sock_path)
            raw.settimeout(2.0)

            from c_two.transport.ipc.frame import encode_frame
            from c_two.transport.protocol import FLAG_CALL_V2

            # FLAG_CALL_V2 frame with only 1 byte payload (name_len=5 but no data).
            malformed_payload = bytes([5])  # name_len=5, but no name or method_idx
            frame = encode_frame(1, FLAG_CALL_V2, malformed_payload)
            raw.sendall(frame)
            # Server should handle error (close conn or return error), not crash.
            time.sleep(0.2)
            raw.close()

            # Verify server is still alive by making a normal call.
            client = SharedClient(addr, cfg)
            client.connect()
            r = client.call('echo', b'still alive')
            assert r == b'still alive'
            client.terminate()
        finally:
            server.shutdown()

    def test_server_survives_zero_length_payload(self):
        """Empty v2 call payload should not crash server."""
        import socket as _sock

        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        try:
            sock_dir = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')
            region = addr.replace('ipc-v3://', '')
            sock_path = os.path.join(sock_dir, region + '.sock')

            raw = _sock.socket(_sock.AF_UNIX, _sock.SOCK_STREAM)
            raw.connect(sock_path)
            raw.settimeout(2.0)

            from c_two.transport.ipc.frame import encode_frame
            from c_two.transport.protocol import FLAG_CALL_V2

            frame = encode_frame(1, FLAG_CALL_V2, b'')
            raw.sendall(frame)
            time.sleep(0.2)
            raw.close()

            # Server should still be alive.
            client = SharedClient(addr, cfg)
            client.connect()
            r = client.call('echo', b'ok')
            assert r == b'ok'
            client.terminate()
        finally:
            server.shutdown()

    def test_server_survives_corrupted_v2_control(self):
        """V2 call with garbage control bytes should not crash server."""
        import socket as _sock

        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        try:
            sock_dir = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')
            region = addr.replace('ipc-v3://', '')
            sock_path = os.path.join(sock_dir, region + '.sock')

            raw = _sock.socket(_sock.AF_UNIX, _sock.SOCK_STREAM)
            raw.connect(sock_path)
            raw.settimeout(2.0)

            from c_two.transport.ipc.frame import encode_frame
            from c_two.transport.protocol import FLAG_CALL_V2

            # Send a v2 call with garbage control bytes (too short to parse)
            frame = encode_frame(1, FLAG_CALL_V2, b'\xff\x01')
            raw.sendall(frame)
            time.sleep(0.2)
            raw.close()

            # Server should still be alive.
            client = SharedClient(addr, cfg)
            client.connect()
            r = client.call('echo', b'ok')
            assert r == b'ok'
            client.terminate()
        finally:
            server.shutdown()

class TestSOTAAPIConcurrency:
    """Test the high-level cc.register/connect/close API under concurrency."""

    def test_concurrent_connect_close_cycles(self):
        """Multiple threads rapidly connect/use/close the same CRM."""
        addr = _next_addr()
        cc.set_address(addr)
        cc.set_ipc_config(segment_size=1 << 20, max_segments=2)
        cc.register(IEcho, EchoImpl(), name='echo_conc')

        try:
            errors = []

            def worker(tid: int):
                try:
                    for _ in range(5):
                        icrm = cc.connect(IEcho, name='echo_conc')
                        r = icrm.echo(f'tid={tid}'.encode())
                        assert r == f'tid={tid}'.encode()
                        cc.close(icrm)
                except Exception as e:
                    errors.append((tid, e))

            threads = []
            for tid in range(8):
                t = threading.Thread(target=worker, args=(tid,))
                threads.append(t)
                t.start()
            for t in threads:
                t.join(timeout=15)

            assert not errors, f'Errors: {errors}'
        finally:
            cc.shutdown()

    def test_connect_unknown_name_raises(self):
        """cc.connect with unknown name raises LookupError."""
        addr = _next_addr()
        cc.set_address(addr)
        cc.register(IEcho, EchoImpl(), name='echo_lookup')

        try:
            with pytest.raises(LookupError, match='not registered'):
                cc.connect(IEcho, name='nonexistent')
        finally:
            cc.shutdown()

    def test_double_register_same_name_raises(self):
        """Registering the same name twice raises ValueError."""
        addr = _next_addr()
        cc.set_address(addr)
        cc.register(IEcho, EchoImpl(), name='echo_dup')

        try:
            with pytest.raises(ValueError, match='already registered'):
                cc.register(IEcho, EchoImpl(), name='echo_dup')
        finally:
            cc.shutdown()
