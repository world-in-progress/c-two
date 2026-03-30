"""Integration tests for chunked streaming transfer (IPC).

Tests verify end-to-end chunked transfer for payloads exceeding the buddy
pool segment size.  Uses a small ``pool_segment_size`` (64 KB) to trigger
chunking with manageable payload sizes (~70-200 KB).

Key thresholds (with pool_segment_size=65536):
    chunk_threshold = 65536 * 0.9 ≈ 58982  (call/reply triggers above this)
    chunk_size      = 65536 // 2  = 32768   (each chunk)
"""

from __future__ import annotations

import os
import pickle
import threading
import time

import pytest

import c_two as cc
from c_two.error import MemoryPressureError
from c_two.transport.ipc.frame import IPCConfig
from c_two.transport.client.core import SharedClient
from c_two.transport.client.proxy import ICRMProxy
from c_two.transport.server.core import Server


# ---------------------------------------------------------------------------
# Inline ICRM / CRM for chunked-transfer testing
# ---------------------------------------------------------------------------

@cc.icrm(namespace='test.chunk_echo', version='0.1.0')
class IChunkTest:
    """ICRM that echoes data and produces large returns."""

    def echo(self, data: str) -> str:
        """Echo the input string back (same size in + out)."""
        ...

    def make_large(self, size: int) -> str:
        """Return a string of exactly *size* characters."""
        ...

    def add(self, a: int, b: int) -> int:
        """Simple arithmetic — used as a small-payload probe."""
        ...


class ChunkTest:
    """CRM implementation for chunked-transfer testing."""

    def echo(self, data: str) -> str:
        return data

    def make_large(self, size: int) -> str:
        return 'X' * size

    def add(self, a: int, b: int) -> int:
        return a + b


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')
_counter = 0

# Pool config that forces chunking at ~58 KB payload.
_SEG_SIZE = 65536       # 64 KB per segment
_MAX_SEGS = 16          # plenty of headroom
_MAX_MEM = _SEG_SIZE * _MAX_SEGS


def _unique_region() -> str:
    global _counter
    _counter += 1
    return f'chunk_test_{os.getpid()}_{_counter}'


def _wait_for_server(addr: str, timeout: float = 5.0) -> None:
    region = addr.replace('ipc://', '')
    sock_path = os.path.join(_IPC_SOCK_DIR, f'{region}.sock')
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.exists(sock_path):
            return
        time.sleep(0.05)
    raise TimeoutError(f'Server {addr} did not start within {timeout}s')


def _chunk_config() -> IPCConfig:
    return IPCConfig(
        pool_segment_size=_SEG_SIZE,
        max_pool_segments=_MAX_SEGS,
        max_pool_memory=_MAX_MEM,
    )


def _setup(
    addr: str | None = None,
    cfg: IPCConfig | None = None,
) -> tuple[str, Server, SharedClient, IChunkTest]:
    """Spin up a Server + SharedClient pair with IChunkTest CRM.

    Returns ``(addr, server, client, icrm)``.
    """
    if addr is None:
        addr = f'ipc://{_unique_region()}'
    if cfg is None:
        cfg = _chunk_config()

    server = Server(bind_address=addr, ipc_config=cfg)
    server.register_crm(IChunkTest, ChunkTest(), name='chunk')
    server.start()
    _wait_for_server(addr)

    client = SharedClient(addr, ipc_config=cfg)
    client.connect()

    proxy = ICRMProxy.ipc(client, 'chunk')
    icrm = IChunkTest()
    icrm.client = proxy

    return addr, server, client, icrm


def _teardown(server: Server, client: SharedClient) -> None:
    try:
        client.terminate()
    except Exception:
        pass
    try:
        server.shutdown()
    except Exception:
        pass


# ===========================================================================
# Basic chunked transfer tests
# ===========================================================================

class TestChunkedCallRoundtrip:
    """Verify large-payload calls are chunked and return correct results."""

    def test_small_call_normal_path(self):
        """Payload below threshold uses normal (non-chunked) path."""
        addr, server, client, icrm = _setup()
        try:
            result = icrm.add(100, 200)
            assert result == 300
        finally:
            _teardown(server, client)

    def test_chunked_call_70kb(self):
        """70 KB payload triggers chunked call (3 chunks × 32 KB)."""
        addr, server, client, icrm = _setup()
        try:
            payload = 'A' * 70_000
            result = icrm.echo(payload)
            assert result == payload
            assert len(result) == 70_000
        finally:
            _teardown(server, client)

    def test_chunked_call_200kb(self):
        """200 KB payload triggers chunked call (7 chunks × 32 KB)."""
        addr, server, client, icrm = _setup()
        try:
            payload = 'B' * 200_000
            result = icrm.echo(payload)
            assert result == payload
        finally:
            _teardown(server, client)

    def test_chunked_call_500kb(self):
        """500 KB payload — 16 chunks with 64 KB pool segments."""
        addr, server, client, icrm = _setup()
        try:
            payload = 'C' * 500_000
            result = icrm.echo(payload)
            assert result == payload
        finally:
            _teardown(server, client)


class TestChunkedReplyRoundtrip:
    """Verify large CRM results are chunked back to client."""

    def test_large_reply_70kb(self):
        """CRM returns 70 KB string — triggers chunked reply."""
        addr, server, client, icrm = _setup()
        try:
            result = icrm.make_large(70_000)
            assert len(result) == 70_000
            assert result == 'X' * 70_000
        finally:
            _teardown(server, client)

    def test_large_reply_200kb(self):
        """CRM returns 200 KB string — multiple reply chunks."""
        addr, server, client, icrm = _setup()
        try:
            result = icrm.make_large(200_000)
            assert len(result) == 200_000
        finally:
            _teardown(server, client)


class TestChunkedBidirectional:
    """Both call args AND reply are chunked."""

    def test_echo_100kb(self):
        """100 KB echo: call chunked, reply chunked, data integrity."""
        addr, server, client, icrm = _setup()
        try:
            payload = 'D' * 100_000
            result = icrm.echo(payload)
            assert result == payload
        finally:
            _teardown(server, client)

    def test_echo_300kb(self):
        """300 KB bidirectional chunked transfer."""
        addr, server, client, icrm = _setup()
        try:
            payload = 'E' * 300_000
            result = icrm.echo(payload)
            assert result == payload
        finally:
            _teardown(server, client)


# ===========================================================================
# Interleaving: chunked + small calls concurrently
# ===========================================================================

class TestChunkedInterleave:
    """Chunked transfers interleaved with small calls from multiple threads."""

    def test_large_and_small_concurrent(self):
        """8 threads: half send large payloads, half send small calls."""
        addr, server, client, _, = _setup()
        try:
            errors: list[Exception] = []
            results_large: list[tuple[int, str]] = []
            results_small: list[tuple[int, int]] = []
            lock = threading.Lock()

            def large_worker(tid: int) -> None:
                try:
                    proxy = ICRMProxy.ipc(client, 'chunk')
                    icrm = IChunkTest()
                    icrm.client = proxy
                    payload = chr(ord('A') + tid) * 80_000
                    result = icrm.echo(payload)
                    with lock:
                        results_large.append((tid, result))
                except Exception as e:
                    with lock:
                        errors.append(e)

            def small_worker(tid: int) -> None:
                try:
                    proxy = ICRMProxy.ipc(client, 'chunk')
                    icrm = IChunkTest()
                    icrm.client = proxy
                    for j in range(5):
                        result = icrm.add(tid, j)
                        with lock:
                            results_small.append((tid, j, result))
                except Exception as e:
                    with lock:
                        errors.append(e)

            threads = []
            for i in range(4):
                threads.append(threading.Thread(target=large_worker, args=(i,)))
                threads.append(threading.Thread(target=small_worker, args=(i + 10,)))

            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=60)

            assert len(errors) == 0, f'Unexpected errors: {errors}'

            # Verify large results.
            assert len(results_large) == 4
            for tid, result in results_large:
                expected = chr(ord('A') + tid) * 80_000
                assert result == expected, f'Large data corruption tid={tid}'

            # Verify small results.
            assert len(results_small) == 20  # 4 threads × 5 calls
            for tid, j, result in results_small:
                assert result == tid + j, f'Small data corruption tid={tid}, j={j}'
        finally:
            _teardown(server, client)


# ===========================================================================
# Capability degradation
# ===========================================================================

class TestChunkedCapability:
    """Tests for chunked capability negotiation edge cases."""

    def test_chunked_capable_flag_set(self):
        """Client._chunked_capable is True after handshake with new server."""
        addr, server, client, _ = _setup()
        try:
            assert client._chunked_capable is True
        finally:
            _teardown(server, client)

    def test_force_disable_raises_memory_pressure(self):
        """If _chunked_capable is forcibly False, large payloads raise error."""
        addr, server, client, icrm = _setup()
        try:
            # Forcibly disable chunked capability.
            client._chunked_capable = False

            payload = 'Z' * 80_000
            with pytest.raises(MemoryPressureError):
                icrm.echo(payload)

            # Small calls still work.
            assert icrm.add(1, 2) == 3
        finally:
            _teardown(server, client)


# ===========================================================================
# Recovery: chunked call followed by normal call
# ===========================================================================

class TestChunkedRecovery:
    """Verify the system recovers to normal operation after chunked transfers."""

    def test_normal_after_chunked(self):
        """Small call succeeds after a large chunked call."""
        addr, server, client, icrm = _setup()
        try:
            # Large call.
            big = 'R' * 100_000
            assert icrm.echo(big) == big

            # Small call immediately after.
            assert icrm.add(42, 58) == 100
            assert icrm.echo('hello') == 'hello'
        finally:
            _teardown(server, client)

    def test_multiple_chunked_sequential(self):
        """Multiple large calls in sequence all succeed."""
        addr, server, client, icrm = _setup()
        try:
            for i in range(5):
                payload = chr(ord('A') + i) * 80_000
                result = icrm.echo(payload)
                assert result == payload
        finally:
            _teardown(server, client)


# ===========================================================================
# Backpressure: chunked under memory pressure
# ===========================================================================

class TestChunkedBackpressure:
    """Test chunked transfer behavior under memory pressure."""

    def test_chunked_with_tight_pool(self):
        """Chunked transfer works even with a very tight pool
        (inline fallback for chunks that can't buddy-alloc)."""
        cfg = IPCConfig(
            pool_segment_size=_SEG_SIZE,
            max_pool_segments=2,       # only 128 KB total
            max_pool_memory=_SEG_SIZE * 2,
        )
        addr, server, client, icrm = _setup(cfg=cfg)
        try:
            # 70 KB payload → 3 chunks of 32 KB; tight pool may cause
            # some chunks to go inline, but transfer should still succeed
            # because each chunk (32 KB) < max_frame_size (16 MB).
            payload = 'T' * 70_000
            result = icrm.echo(payload)
            assert result == payload
        finally:
            _teardown(server, client)
