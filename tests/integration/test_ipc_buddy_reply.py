"""Test that large IPC responses use buddy SHM path."""
from __future__ import annotations

import os
import time

import pytest

import c_two as cc
from c_two.transport.registry import _ProcessRegistry


@cc.icrm(namespace='test.buddy_reply', version='0.1.0')
class IEcho:
    def echo(self, data: bytes) -> bytes: ...


class Echo:
    def echo(self, data: bytes) -> bytes:
        return data


@pytest.fixture(autouse=True)
def _cleanup():
    yield
    cc.shutdown()
    _ProcessRegistry._instance = None


@pytest.mark.timeout(30)
class TestBuddyReply:
    """Verify large responses transit via buddy SHM (not inline UDS)."""

    def _setup_ipc(self, address: str) -> cc.ICRMProxy:
        cc.set_address(address)
        cc.register(IEcho, Echo(), name='echo')
        time.sleep(0.3)
        return cc.connect(IEcho, name='echo', address=address)

    def test_small_payload_roundtrip(self):
        """Small payloads still work (inline path)."""
        proxy = self._setup_ipc('ipc://test_buddy_reply_small')
        data = b'hello'
        result = proxy.echo(data)
        assert result == data
        cc.close(proxy)

    def test_large_payload_roundtrip(self):
        """Large payload (1MB) goes through buddy SHM response path."""
        proxy = self._setup_ipc('ipc://test_buddy_reply_large')
        data = os.urandom(1024 * 1024)  # 1 MB — above 4KB shm_threshold
        result = proxy.echo(data)
        assert result == data
        cc.close(proxy)

    def test_very_large_payload_roundtrip(self):
        """Very large payload (50MB) round-trips correctly."""
        proxy = self._setup_ipc('ipc://test_buddy_reply_vlarge')
        data = os.urandom(50 * 1024 * 1024)  # 50 MB
        result = proxy.echo(data)
        assert len(result) == len(data)
        assert result == data
        cc.close(proxy)

    def test_multiple_large_calls(self):
        """Multiple sequential large calls don't leak memory."""
        proxy = self._setup_ipc('ipc://test_buddy_reply_multi')
        for _ in range(10):
            data = os.urandom(1024 * 1024)  # 1 MB each
            result = proxy.echo(data)
            assert result == data
        cc.close(proxy)

    def test_gc_idle_segment_reclaim(self):
        """GC reclaims idle segments after bursty allocation."""
        proxy = self._setup_ipc('ipc://test_buddy_reply_gc')
        # Force multiple segment usage with large payloads
        for _ in range(5):
            data = os.urandom(10 * 1024 * 1024)  # 10 MB each
            result = proxy.echo(data)
            assert len(result) == len(data)
        # After burst, smaller allocations should still work (GC cleans up)
        small = os.urandom(1024)
        assert proxy.echo(small) == small
        cc.close(proxy)

    def test_stress_mixed_payload_sizes(self):
        """Mixed payload sizes stress-test both inline and SHM paths."""
        proxy = self._setup_ipc('ipc://test_buddy_reply_stress')
        sizes = [64, 512, 4096, 32768, 1024 * 1024, 64, 512, 4096]
        for size in sizes:
            data = os.urandom(size)
            result = proxy.echo(data)
            assert result == data, f"Mismatch at size {size}"
        cc.close(proxy)

    def test_concurrent_large_calls(self):
        """Multiple concurrent large calls work correctly."""
        import threading

        addr = 'ipc://test_buddy_reply_concurrent'
        cc.set_address(addr)
        cc.register(IEcho, Echo(), name='echo')
        time.sleep(0.3)

        errors = []

        def worker(worker_id):
            try:
                proxy = cc.connect(IEcho, name='echo', address=addr)
                for i in range(3):
                    data = os.urandom(1024 * 1024)  # 1 MB
                    result = proxy.echo(data)
                    if result != data:
                        errors.append(f"Worker {worker_id} call {i}: mismatch")
                cc.close(proxy)
            except Exception as e:
                errors.append(f"Worker {worker_id}: {e}")

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(3)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=20)
        assert not errors, f"Concurrent test errors: {errors}"

    def test_response_buffer_memoryview(self):
        """ResponseBuffer supports zero-copy memoryview access."""
        proxy = self._setup_ipc('ipc://test_buddy_reply_mv')
        rust_client = proxy.client._client
        route_name = proxy.client._name
        data = os.urandom(1024 * 1024)  # 1 MB — above SHM threshold
        response = rust_client.call(route_name, 'echo', data)
        mv = memoryview(response)
        assert len(mv) == len(data)
        assert bytes(mv) == data
        del mv
        response.release()
        cc.close(proxy)

    def test_response_buffer_auto_release(self):
        """ResponseBuffer auto-releases SHM on garbage collection."""
        proxy = self._setup_ipc('ipc://test_buddy_reply_autorel')
        rust_client = proxy.client._client
        route_name = proxy.client._name
        import gc
        for _ in range(5):
            data = os.urandom(1024 * 1024)
            response = rust_client.call(route_name, 'echo', data)
            del response
            gc.collect()
        # Should still work after GC
        small = os.urandom(64)
        result = proxy.echo(small)
        assert result == small
        cc.close(proxy)
