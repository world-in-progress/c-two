"""Integration tests for zero-copy SHM IPC path."""
from __future__ import annotations

import os
import time

import pytest

import c_two as cc
from c_two.transport.registry import _ProcessRegistry


@cc.crm(namespace='test.zero_copy', version='0.1.0')
class IEchoZC:
    def echo(self, data: bytes) -> bytes: ...


class EchoZC:
    def echo(self, data: bytes) -> bytes:
        return data


@pytest.fixture(autouse=True)
def _cleanup():
    yield
    cc.shutdown()
    _ProcessRegistry._instance = None


def _setup():
    cc.register(IEchoZC, EchoZC(), name='echo_zc')
    time.sleep(0.3)
    address = cc.server_address()
    return cc.connect(IEchoZC, name='echo_zc', address=address)


@pytest.mark.timeout(30)
class TestZeroCopyIPC:
    """Verify zero-copy data path across payload size tiers."""

    def test_inline_small(self):
        """Payloads below SHM threshold use inline path (64B, 512B)."""
        proxy = _setup()
        for size in [64, 512]:
            data = os.urandom(size)
            result = proxy.echo(data)
            assert result == data, f"Mismatch at {size}B"
        cc.close(proxy)

    def test_buddy_medium(self):
        """Medium payloads (1KB-1MB) use buddy SHM zero-copy path."""
        proxy = _setup()
        for size in [1024, 64 * 1024, 1024 * 1024]:
            data = os.urandom(size)
            result = proxy.echo(data)
            assert result == data, f"Mismatch at {size}B"
        cc.close(proxy)

    def test_large_payload(self):
        """Large payloads (10MB) use buddy/dedicated SHM."""
        proxy = _setup()
        data = os.urandom(10 * 1024 * 1024)
        result = proxy.echo(data)
        assert result == data
        cc.close(proxy)

    def test_data_integrity_pattern(self):
        """Verify specific byte patterns survive the zero-copy path."""
        proxy = _setup()
        # All zeros
        data = b'\x00' * 65536
        assert proxy.echo(data) == data
        # All ones
        data = b'\xff' * 65536
        assert proxy.echo(data) == data
        # Repeating pattern
        data = bytes(range(256)) * 256
        assert proxy.echo(data) == data
        cc.close(proxy)

    def test_sequential_calls_no_leak(self):
        """Multiple sequential calls don't leak SHM blocks."""
        proxy = _setup()
        for _ in range(20):
            data = os.urandom(1024 * 1024)
            result = proxy.echo(data)
            assert len(result) == len(data)
        cc.close(proxy)

    def test_empty_payload(self):
        """Empty payload works correctly."""
        proxy = _setup()
        result = proxy.echo(b'')
        assert result == b''
        cc.close(proxy)

    def test_mixed_sizes(self):
        """Alternating small and large payloads work correctly."""
        proxy = _setup()
        sizes = [64, 1024 * 1024, 128, 512 * 1024, 32, 2 * 1024 * 1024]
        for size in sizes:
            data = os.urandom(size)
            result = proxy.echo(data)
            assert result == data, f"Mismatch at {size}B"
        cc.close(proxy)
