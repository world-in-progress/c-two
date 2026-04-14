"""Concurrency safety and security integration tests.

Tests:
- Thread-safe concurrent calls via SOTA API
- Server double-shutdown safety
- Concurrent connect/close lifecycle
"""
from __future__ import annotations

import os
import time
import threading

import pytest

import c_two as cc
from c_two.transport.server import Server
from c_two.config.ipc import ServerIPCConfig
from c_two.transport.client.util import ping


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
    """Poll until server responds to ping."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if ping(addr, timeout=0.5):
            return
        time.sleep(0.02)
    raise TimeoutError(f'Server at {addr} not ready within {timeout}s')


# ---------------------------------------------------------------------------
# Concurrent calls stress test via SOTA API
# ---------------------------------------------------------------------------

class TestConcurrentCallSafety:
    """Stress-test concurrent call safety via SOTA API."""

    def test_32_threads_concurrent_echo(self):
        """32 threads making concurrent echo calls via a shared proxy."""
        addr = _next_addr()
        cfg = ServerIPCConfig(
            pool_segment_size=1 << 20,  # 1MB
            max_pool_segments=2,
            max_pool_memory=2 << 20,
        )
        server = Server(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl(), name='echo')
        server.start()
        _wait_for_server(addr)

        try:
            proxy = cc.connect(IEcho, name='echo', address=addr)

            errors = []
            results = {}

            def worker(thread_id: int, calls: int):
                try:
                    for i in range(calls):
                        tag = f'{thread_id}:{i}'.encode()
                        r = proxy.echo(tag)
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
            cc.close(proxy)
        finally:
            server.shutdown()

    def test_concurrent_calls_different_sizes(self):
        """Concurrent calls with varied payload sizes (inline + buddy paths)."""
        addr = _next_addr()
        cfg = ServerIPCConfig(
            pool_segment_size=1 << 20,
            max_pool_segments=2,
            max_pool_memory=2 << 20,
        )
        server = Server(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl(), name='echo')
        server.start()
        _wait_for_server(addr)

        try:
            proxy = cc.connect(IEcho, name='echo', address=addr)

            errors = []
            sizes = [64, 256, 1024, 4096, 16384, 65536]

            def worker(thread_id: int, payload_size: int):
                try:
                    data = bytes(range(256)) * (payload_size // 256 + 1)
                    data = data[:payload_size]
                    for _ in range(5):
                        r = proxy.echo(data)
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
            cc.close(proxy)
        finally:
            server.shutdown()


# ---------------------------------------------------------------------------
# Double-shutdown safety
# ---------------------------------------------------------------------------

class TestDoubleShutdown:
    """Ensure calling shutdown/close multiple times is safe."""

    def test_proxy_double_close(self):
        addr = _next_addr()
        server = Server(bind_address=addr)
        server.register_crm(IEcho, EchoImpl(), name='echo')
        server.start()
        _wait_for_server(addr)

        try:
            proxy = cc.connect(IEcho, name='echo', address=addr)
            r = proxy.echo(b'hello')
            assert r == b'hello'

            # Close twice — should not raise.
            cc.close(proxy)
            cc.close(proxy)
        finally:
            server.shutdown()

    def test_server_double_shutdown(self):
        addr = _next_addr()
        cfg = ServerIPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        server = Server(bind_address=addr, ipc_config=cfg)
        server.register_crm(IEcho, EchoImpl(), name='echo')
        server.start()
        _wait_for_server(addr)

        # Shutdown twice — should not raise.
        server.shutdown()
        server.shutdown()


class TestSOTAAPIConcurrency:
    """Test the high-level cc.register/connect/close API under concurrency."""

    def test_concurrent_connect_close_cycles(self):
        """Multiple threads rapidly connect/use/close the same CRM."""
        cc.set_server(pool_segment_size=1 << 20, max_pool_segments=2)
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
        cc.register(IEcho, EchoImpl(), name='echo_lookup')

        try:
            with pytest.raises(LookupError, match='not registered'):
                cc.connect(IEcho, name='nonexistent')
        finally:
            cc.shutdown()

    def test_double_register_same_name_raises(self):
        """Registering the same name twice raises ValueError."""
        cc.register(IEcho, EchoImpl(), name='echo_dup')

        try:
            with pytest.raises(ValueError, match='already registered'):
                cc.register(IEcho, EchoImpl(), name='echo_dup')
        finally:
            cc.shutdown()
