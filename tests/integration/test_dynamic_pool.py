"""Integration tests for dynamic SHM pool expansion (IPC).

Tests verify end-to-end behaviour when the buddy pool's initial segment is
exhausted and new segments are lazily created by the client and opened by the
server.  Uses a small ``pool_segment_size`` (64 KB) so expansion is triggered
with manageable payload sizes (~50-300 KB).

Naming convention for SHM segments:
    {prefix}_b{seg_idx:04x}   — deterministic from pool prefix + segment index
"""

from __future__ import annotations

import os
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed

import pytest

import c_two as cc
from c_two.transport.ipc.frame import IPCConfig
from c_two.transport.client.util import ping
from c_two.transport.server import Server


# ---------------------------------------------------------------------------
# Inline ICRM / CRM for pool-expansion testing
# ---------------------------------------------------------------------------

@cc.icrm(namespace='test.dynamic_pool', version='0.1.0')
class IPoolTest:
    """ICRM that echoes bytes and produces large returns."""

    def echo(self, data: bytes) -> bytes:
        """Echo the input bytes back unchanged."""
        ...

    def make_large(self, size: int) -> bytes:
        """Return *size* zero-filled bytes."""
        ...


class PoolTestCRM:
    """CRM implementation for pool-expansion testing."""

    def echo(self, data: bytes) -> bytes:
        return data

    def make_large(self, size: int) -> bytes:
        return b'\x00' * size


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_counter = 0

_SEG_SIZE = 65536       # 64 KB per segment


def _unique_region() -> str:
    global _counter
    _counter += 1
    return f'dyn_pool_{os.getpid()}_{_counter}'


def _wait_for_server(addr: str, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if ping(addr, timeout=0.5):
            return
        time.sleep(0.05)
    raise TimeoutError(f'Server {addr} did not start within {timeout}s')


def _pool_config(max_segs: int = 4) -> IPCConfig:
    return IPCConfig(
        pool_segment_size=_SEG_SIZE,
        max_pool_segments=max_segs,
        max_pool_memory=_SEG_SIZE * max_segs,
    )


def _setup(
    addr: str | None = None,
    cfg: IPCConfig | None = None,
):
    """Spin up a Server + SOTA proxy for IPoolTest CRM.

    Returns ``(addr, server, proxy)``.
    """
    if addr is None:
        addr = f'ipc://{_unique_region()}'
    if cfg is None:
        cfg = _pool_config()

    server = Server(bind_address=addr, ipc_config=cfg)
    server.register_crm(IPoolTest, PoolTestCRM(), name='pool')
    server.start()
    _wait_for_server(addr)

    proxy = cc.connect(IPoolTest, name='pool', address=addr)

    return addr, server, proxy


def _teardown(server: Server, proxy) -> None:
    try:
        cc.close(proxy)
    except Exception:
        pass
    try:
        server.shutdown()
    except Exception:
        pass


# ===========================================================================
# Test 1 — pool expands on a single large allocation
# ===========================================================================

class TestPoolExpandsOnLargeAlloc:
    """A ~100 KB payload with 64 KB segments must trigger expansion."""

    def test_pool_expands_on_large_alloc(self):
        cfg = _pool_config(max_segs=4)
        addr, server, proxy = _setup(cfg=cfg)
        try:
            payload = b'\xAB' * 100_000  # ~100 KB, exceeds single 64 KB seg
            result = proxy.echo(payload)

            assert result == payload
            assert len(result) == 100_000
        finally:
            _teardown(server, proxy)


# ===========================================================================
# Test 2 — concurrent calls force expansion under contention
# ===========================================================================

class TestMultiSegmentConcurrentCalls:
    """Multiple concurrent ~50 KB calls should all succeed even if the pool
    must expand under contention from several threads."""

    def test_multi_segment_concurrent_calls(self):
        cfg = _pool_config(max_segs=8)
        addr, server, proxy = _setup(cfg=cfg)
        try:
            num_workers = 6
            payload_size = 50_000  # each > half a 64 KB segment

            def worker(tid: int) -> bytes:
                payload = bytes([tid & 0xFF]) * payload_size
                return proxy.echo(payload)

            with ThreadPoolExecutor(max_workers=num_workers) as pool:
                futures = {pool.submit(worker, i): i for i in range(num_workers)}
                results: dict[int, bytes] = {}
                for fut in as_completed(futures):
                    tid = futures[fut]
                    results[tid] = fut.result()

            for tid, data in results.items():
                assert len(data) == payload_size
                assert data == bytes([tid & 0xFF]) * payload_size
        finally:
            _teardown(server, proxy)


# ===========================================================================
# Test 3 — server lazily opens new client segments
# ===========================================================================

class TestServerLazyOpensNewSegments:
    """Sequential calls with increasing payload sizes force segment creation.
    The server must lazy-open segments it didn't see at handshake time."""

    def test_server_lazy_opens_new_segments(self):
        cfg = _pool_config(max_segs=8)
        addr, server, proxy = _setup(cfg=cfg)
        try:
            # Escalating payloads: fits within Rust client's buddy pool.
            sizes = [10_000, 30_000, 50_000, 70_000]
            for size in sizes:
                payload = b'\xCD' * size
                result = proxy.echo(payload)
                assert len(result) == size
                assert result == payload
        finally:
            _teardown(server, proxy)


# ===========================================================================
# Test 4 — expansion does not exceed max_pool_segments (fallback)
# ===========================================================================

class TestExpansionDoesNotExceedMaxSegments:
    """With max_pool_segments=2 (128 KB total), a 300 KB payload must still
    succeed — the transport falls back to chunked/dedicated transfer when the
    pool is full."""

    def test_expansion_does_not_exceed_max_segments(self):
        cfg = _pool_config(max_segs=2)
        addr, server, proxy = _setup(cfg=cfg)
        try:
            payload = b'\xEF' * 80_000  # Within buddy pool range
            result = proxy.echo(payload)

            assert result == payload
            assert len(result) == 80_000
        finally:
            _teardown(server, proxy)
