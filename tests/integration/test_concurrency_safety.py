"""Concurrency safety and security integration tests.

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
from c_two.transport.server import Server
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
    return f'ipc://test_conc_{os.getpid()}_{_counter}'


def _wait_for_server(addr: str, timeout: float = 3.0):
    """Poll until server socket is ready."""
    import socket as _sock
    sock_dir = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')
    region = addr.replace('ipc://', '')
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
        server = Server(bind_address=addr, ipc_config=cfg)
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
        server = Server(bind_address=addr, ipc_config=cfg)
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
        server = Server(bind_address=addr, ipc_config=cfg)
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
        server = Server(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        # Shutdown twice — should not raise.
        server.shutdown()
        server.shutdown()


# ---------------------------------------------------------------------------
# Call after terminate raises cleanly
# ---------------------------------------------------------------------------

class TestCallAfterTerminate:
    """Verify calling a terminated client reconnects cleanly."""

    def test_call_after_terminate_reconnects(self):
        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        server = Server(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        try:
            client = SharedClient(addr, cfg)
            client.connect()
            r = client.call('echo', b'ok')
            assert r == b'ok'

            client.terminate()

            # Thin wrapper reconnects automatically on next call.
            r2 = client.call('echo', b'reconnected')
            assert r2 == b'reconnected'
        finally:
            server.shutdown()


# ---------------------------------------------------------------------------
# Malformed frame handling — server stays alive
# ---------------------------------------------------------------------------

class TestServerMalformedFrames:
    """Verify server gracefully handles malformed frames without crashing."""

    def test_server_survives_malformed_payload(self):
        """Send truncated call control; server should disconnect client
        but keep serving others."""
        import socket as _sock

        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        server = Server(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        try:
            sock_dir = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')
            region = addr.replace('ipc://', '')
            sock_path = os.path.join(sock_dir, region + '.sock')

            # Send a malformed call frame (truncated call control).
            raw = _sock.socket(_sock.AF_UNIX, _sock.SOCK_STREAM)
            raw.connect(sock_path)
            raw.settimeout(2.0)

            from c_two.transport.ipc.frame import encode_frame
            from c_two.transport.protocol import FLAG_CALL

            # FLAG_CALL frame with only 1 byte payload (name_len=5 but no data).
            malformed_payload = bytes([5])  # name_len=5, but no name or method_idx
            frame = encode_frame(1, FLAG_CALL, malformed_payload)
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
        """Empty call payload should not crash server."""
        import socket as _sock

        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        server = Server(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        try:
            sock_dir = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')
            region = addr.replace('ipc://', '')
            sock_path = os.path.join(sock_dir, region + '.sock')

            raw = _sock.socket(_sock.AF_UNIX, _sock.SOCK_STREAM)
            raw.connect(sock_path)
            raw.settimeout(2.0)

            from c_two.transport.ipc.frame import encode_frame
            from c_two.transport.protocol import FLAG_CALL

            frame = encode_frame(1, FLAG_CALL, b'')
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

    def test_server_survives_corrupted_control(self):
        """Routed call with garbage control bytes should not crash server."""
        import socket as _sock

        addr = _next_addr()
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        server = Server(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl())
        server.start()
        _wait_for_server(addr)

        try:
            sock_dir = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')
            region = addr.replace('ipc://', '')
            sock_path = os.path.join(sock_dir, region + '.sock')

            raw = _sock.socket(_sock.AF_UNIX, _sock.SOCK_STREAM)
            raw.connect(sock_path)
            raw.settimeout(2.0)

            from c_two.transport.ipc.frame import encode_frame
            from c_two.transport.protocol import FLAG_CALL

            # Send a call with garbage control bytes (too short to parse)
            frame = encode_frame(1, FLAG_CALL, b'\xff\x01')
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
