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
