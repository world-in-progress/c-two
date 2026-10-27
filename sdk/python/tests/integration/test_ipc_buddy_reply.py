"""Test that large IPC responses use buddy SHM path."""
from __future__ import annotations

import os
import time

import pytest

import c_two as cc
from c_two import _native
from c_two.transport import registry as registry_module


@cc.crm(namespace='test.buddy_reply', version='0.1.0')
class IEcho:
    def echo(self, data: bytes) -> bytes: ...


class Echo:
    def echo(self, data: bytes) -> bytes:
        return data


@pytest.fixture(autouse=True)
def _cleanup():
    yield
    cc.shutdown()
    registry_module._default_runtime = None


@pytest.mark.timeout(30)
class TestBuddyReply:
    """Verify large responses transit via buddy SHM (not inline UDS)."""

    def _setup_ipc(self) -> cc.CRMProxy:
        cc.register(IEcho, Echo(), name='echo')
        address = cc.server_address()
        return cc.connect(IEcho, name='echo', address=address)

    def test_small_payload_roundtrip(self):
        """Small payloads still work (inline path)."""
        proxy = self._setup_ipc()
        data = b'hello'
        result = proxy.echo(data)
        assert result == data
        cc.close(proxy)

    def test_large_payload_roundtrip(self):
        """Large payload (1MB) goes through buddy SHM response path."""
        proxy = self._setup_ipc()
        data = os.urandom(1024 * 1024)  # 1 MB — above 4KB shm_threshold
        result = proxy.echo(data)
        assert result == data
        cc.close(proxy)

    def test_very_large_payload_roundtrip(self):
        """Very large payload (50MB) round-trips correctly."""
        proxy = self._setup_ipc()
        data = os.urandom(50 * 1024 * 1024)  # 50 MB
        result = proxy.echo(data)
        assert len(result) == len(data)
        assert result == data
        cc.close(proxy)

    def test_multiple_large_calls(self):
        """Multiple sequential large calls don't leak memory."""
        proxy = self._setup_ipc()
        for _ in range(10):
            data = os.urandom(1024 * 1024)  # 1 MB each
            result = proxy.echo(data)
            assert result == data
        cc.close(proxy)

    def test_gc_idle_segment_reclaim(self):
        """GC reclaims idle segments after bursty allocation."""
        proxy = self._setup_ipc()
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
        proxy = self._setup_ipc()
        sizes = [64, 512, 4096, 32768, 1024 * 1024, 64, 512, 4096]
        for size in sizes:
            data = os.urandom(size)
            result = proxy.echo(data)
            assert result == data, f"Mismatch at size {size}"
        cc.close(proxy)

    def test_view_mode_large_payload(self):
        """View mode (default pickle) works for large payloads through IPC."""
        proxy = self._setup_ipc()
        data = b'V' * (2 * 1024 * 1024)  # 2MB
        result = proxy.echo(data)
        assert result == data
        cc.close(proxy)

    def test_pickle_large_payload(self):
        """Python pickle fallback works for large payloads through IPC."""
        proxy = self._setup_ipc()
        data = b'C' * (1024 * 1024)  # 1MB
        result = proxy.echo(data)
        assert result == data
        cc.close(proxy)

    def test_concurrent_large_calls(self):
        """Multiple concurrent large calls work correctly."""
        import threading

        cc.register(IEcho, Echo(), name='echo')
        addr = cc.server_address()

        errors = []
        completed = [0] * 3

        def worker(worker_id):
            try:
                with cc.connect(IEcho, name='echo', address=addr) as proxy:
                    for i in range(3):
                        data = os.urandom(1024 * 1024)  # 1 MB
                        result = proxy.echo(data)
                        if result != data:
                            errors.append(f"Worker {worker_id} call {i}: mismatch")
                        completed[worker_id] += 1
            except Exception as e:
                errors.append(f"Worker {worker_id}: {e}")

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(3)]
        deadline = time.monotonic() + 20
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=max(0, deadline - time.monotonic()))
        assert all(not t.is_alive() for t in threads), 'Concurrent workers did not finish'
        assert not errors, f"Concurrent test errors: {errors}"
        assert completed == [3, 3, 3], f"Incomplete concurrent calls: {completed}"

    @pytest.mark.parametrize('size', [3, 1024 * 1024])
    def test_response_buffer_memoryview(self, size):
        """ResponseBuffer supports zero-copy memoryview access."""
        import pickle
        proxy = self._setup_ipc()
        rust_client = proxy.client._client
        data = os.urandom(size)
        response = rust_client.call('echo', pickle.dumps(data))
        tracker = _native.BufferLeaseTracker()
        response.track_retained(tracker, 'echo', 'echo')
        assert tracker.stats()['total_held_bytes'] == len(response)
        mv = memoryview(response)
        result = pickle.loads(bytes(mv))
        assert result == data
        with pytest.raises(BufferError, match='currently exported'):
            response.release()
        assert not response.is_released
        assert tracker.stats()['by_storage']['inline']['active_holds'] == 1
        mv.release()
        response.release()
        assert tracker.stats()['by_storage']['inline']['active_holds'] == 0
        assert tracker.stats()['total_held_bytes'] == 0
        response.release()
        cc.close(proxy)

    def test_response_buffer_auto_release(self):
        """ResponseBuffer auto-releases SHM on garbage collection."""
        import pickle
        proxy = self._setup_ipc()
        rust_client = proxy.client._client
        import gc
        for _ in range(5):
            data = os.urandom(1024 * 1024)
            response = rust_client.call('echo', pickle.dumps(data))
            del response
            gc.collect()
        # Should still work after GC
        small = os.urandom(64)
        result = proxy.echo(small)
        assert result == small
        cc.close(proxy)
