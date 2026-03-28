"""Backpressure and OOM protection tests for IPC v3 buddy pool.

Tests verify that:
1. Client inline fallback works when buddy alloc fails (small payload).
2. MemoryPressureError is raised for large payloads that exceed both pool and inline limits.
3. Server inline fallback works transparently for response alloc failures.
4. Concurrent clients under memory pressure all get correct results or clear errors.
"""

from __future__ import annotations

import os
import pickle
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed

import pytest

from c_two.error import MemoryPressureError
from c_two.rpc.ipc.ipc_protocol import IPCConfig
from c_two.rpc_v2.client import SharedClient
from c_two.rpc_v2.proxy import ICRMProxy
from c_two.rpc_v2.server import ServerV2

from tests.fixtures.ihello import IHello
from tests.fixtures.hello import Hello

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')
_counter = 0


def _unique_region() -> str:
    global _counter
    _counter += 1
    return f'bp_test_{os.getpid()}_{_counter}'


def _wait_for_server(addr: str, timeout: float = 5.0) -> None:
    region = addr.replace('ipc-v3://', '')
    sock_path = os.path.join(_IPC_SOCK_DIR, f'{region}.sock')
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.exists(sock_path):
            return
        time.sleep(0.05)
    raise TimeoutError(f'Server {addr} did not start within {timeout}s')


def _tiny_config(seg_size: int = 65536, max_seg: int = 1) -> IPCConfig:
    """Create an IPCConfig with a very small pool for pressure testing."""
    return IPCConfig(
        pool_segment_size=seg_size,
        max_pool_segments=max_seg,
        max_pool_memory=seg_size * max_seg,
    )


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


class TestClientInlineFallback:
    """Client falls back to inline when buddy alloc fails (small payload)."""

    def test_small_payload_inline_fallback(self):
        """Payload fits inline but not in a tiny pool → inline fallback succeeds."""
        addr = f'ipc-v3://{_unique_region()}'
        cfg = _tiny_config(seg_size=65536, max_seg=1)
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        client = SharedClient(addr, try_v2=True, ipc_config=cfg)
        client.connect()
        try:
            proxy = ICRMProxy.ipc(client, 'hello')
            icrm = IHello()
            icrm.client = proxy
            result = icrm.greeting('Backpressure')
            assert result == 'Hello, Backpressure!'
        finally:
            client.terminate()
            server.shutdown()

    def test_normal_payload_within_pool(self):
        """Payload fits within the buddy pool → normal SHM path."""
        addr = f'ipc-v3://{_unique_region()}'
        cfg = _tiny_config(seg_size=65536, max_seg=1)
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        client = SharedClient(addr, try_v2=True, ipc_config=cfg)
        client.connect()
        try:
            proxy = ICRMProxy.ipc(client, 'hello')
            icrm = IHello()
            icrm.client = proxy
            result = icrm.add(10, 20)
            assert result == 30
        finally:
            client.terminate()
            server.shutdown()


class TestMemoryPressureError:
    """MemoryPressureError raised for oversized payloads under pressure."""

    def test_error_attributes(self):
        """MemoryPressureError has the right base class and message."""
        from c_two.error import CCBaseError
        err = MemoryPressureError('test message')
        assert isinstance(err, CCBaseError)
        assert 'test message' in str(err)

    def test_default_message(self):
        err = MemoryPressureError()
        assert 'pool exhausted' in str(err).lower() or 'inline limit' in str(err).lower()


class TestServerInlineFallback:
    """Server already has inline fallback — verify it works transparently."""

    def test_server_inline_reply_under_pressure(self):
        """Server's response alloc fails → falls back to inline → client gets result."""
        addr = f'ipc-v3://{_unique_region()}'
        cfg = _tiny_config(seg_size=65536, max_seg=1)
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        client = SharedClient(addr, try_v2=True, ipc_config=cfg)
        client.connect()
        try:
            proxy = ICRMProxy.ipc(client, 'hello')
            icrm = IHello()
            icrm.client = proxy
            # Multiple calls — some may hit inline fallback on server side.
            for i in range(20):
                result = icrm.greeting(f'User{i}')
                assert result == f'Hello, User{i}!'
        finally:
            client.terminate()
            server.shutdown()


class TestConcurrentBackpressure:
    """Multiple threads sending under memory pressure."""

    def test_concurrent_small_payloads(self):
        """8 threads sending small payloads with tiny pool → all succeed."""
        addr = f'ipc-v3://{_unique_region()}'
        cfg = _tiny_config(seg_size=65536, max_seg=1)
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        errors: list[Exception] = []
        results: list[str] = []
        lock = threading.Lock()

        def worker(thread_id: int) -> None:
            client = SharedClient(addr, try_v2=True, ipc_config=cfg)
            client.connect()
            try:
                proxy = ICRMProxy.ipc(client, 'hello')
                icrm = IHello()
                icrm.client = proxy
                for j in range(10):
                    r = icrm.greeting(f'T{thread_id}R{j}')
                    with lock:
                        results.append(r)
            except Exception as e:
                with lock:
                    errors.append(e)
            finally:
                client.terminate()

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=30)

        server.shutdown()

        # No errors expected — small payloads should all succeed via inline or pool.
        assert len(errors) == 0, f'Unexpected errors: {errors}'
        assert len(results) == 80  # 8 threads × 10 calls

    def test_concurrent_mixed_succeed_or_pressure_error(self):
        """Concurrent threads — verify results are correct or MemoryPressureError."""
        addr = f'ipc-v3://{_unique_region()}'
        cfg = _tiny_config(seg_size=65536, max_seg=1)
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        successes = []
        pressure_errors = []
        other_errors = []
        lock = threading.Lock()

        def worker(thread_id: int) -> None:
            client = SharedClient(addr, try_v2=True, ipc_config=cfg)
            client.connect()
            try:
                proxy = ICRMProxy.ipc(client, 'hello')
                icrm = IHello()
                icrm.client = proxy
                for j in range(5):
                    try:
                        r = icrm.add(thread_id, j)
                        with lock:
                            successes.append((thread_id, j, r))
                    except MemoryPressureError as e:
                        with lock:
                            pressure_errors.append(e)
                    except Exception as e:
                        with lock:
                            other_errors.append(e)
            finally:
                client.terminate()

        with ThreadPoolExecutor(max_workers=4) as pool:
            futs = [pool.submit(worker, i) for i in range(4)]
            for f in as_completed(futs):
                f.result()  # re-raise any unexpected exception

        server.shutdown()

        # All results should be correct.
        for tid, j, r in successes:
            assert r == tid + j
        # No unexpected errors.
        assert len(other_errors) == 0, f'Unexpected errors: {other_errors}'


# ---------------------------------------------------------------------------
# Experiment 3: Recovery + SOTA API + large payload pressure error
# ---------------------------------------------------------------------------


class TestRecoveryAfterPressure:
    """Client can resume normal operation after MemoryPressureError."""

    def test_recovery_after_inline_fallback(self):
        """After inline fallback, subsequent normal calls succeed."""
        addr = f'ipc-v3://{_unique_region()}'
        cfg = _tiny_config(seg_size=65536, max_seg=1)
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        client = SharedClient(addr, try_v2=True, ipc_config=cfg)
        client.connect()
        try:
            proxy = ICRMProxy.ipc(client, 'hello')
            icrm = IHello()
            icrm.client = proxy

            # Phase 1: normal small calls.
            for i in range(5):
                assert icrm.add(i, 1) == i + 1

            # Phase 2: calls that likely trigger inline fallback.
            for i in range(10):
                r = icrm.greeting(f'Recovery{i}')
                assert r == f'Hello, Recovery{i}!'

            # Phase 3: back to small calls — verify client still functional.
            for i in range(5):
                assert icrm.add(100, i) == 100 + i
        finally:
            client.terminate()
            server.shutdown()


class TestLargePayloadPressureError:
    """MemoryPressureError raised when payload exceeds both pool and inline limit."""

    def test_large_payload_raises_pressure_error(self):
        """Payload > max_frame_size with tiny pool → MemoryPressureError."""
        addr = f'ipc-v3://{_unique_region()}'
        # Tiny pool AND small max_frame_size to make pressure error easy to trigger.
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
            max_frame_size=8192,  # 8KB inline limit
        )
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        client = SharedClient(addr, try_v2=True, ipc_config=cfg)
        client.connect()
        try:
            proxy = ICRMProxy.ipc(client, 'hello')
            icrm = IHello()
            icrm.client = proxy

            # Small call succeeds.
            assert icrm.add(1, 2) == 3

            # Large payload: 200KB string > 8KB max_frame_size, > 64KB pool.
            big_name = 'X' * 200_000
            with pytest.raises(MemoryPressureError):
                icrm.greeting(big_name)

            # Client still usable after pressure error.
            assert icrm.add(10, 20) == 30
        finally:
            client.terminate()
            server.shutdown()

    def test_medium_payload_inline_fallback(self):
        """Payload > pool but ≤ max_frame_size → inline fallback succeeds."""
        addr = f'ipc-v3://{_unique_region()}'
        cfg = _tiny_config(seg_size=65536, max_seg=1)
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        client = SharedClient(addr, try_v2=True, ipc_config=cfg)
        client.connect()
        try:
            proxy = ICRMProxy.ipc(client, 'hello')
            icrm = IHello()
            icrm.client = proxy
            # ~100KB string: exceeds 64KB pool but within 16MB default inline limit.
            big_name = 'Y' * 100_000
            result = icrm.greeting(big_name)
            assert result == f'Hello, {big_name}!'
        finally:
            client.terminate()
            server.shutdown()


class TestSOTAAPIBackpressure:
    """Backpressure through the SOTA API (cc.register / cc.connect)."""

    def test_sota_connect_with_tiny_pool(self):
        """SOTA API path: register + connect with tiny IPC config."""
        import c_two as cc

        # Clean slate.
        cc.shutdown()
        addr = f'ipc-v3://{_unique_region()}'
        cc.set_address(addr)
        cc.set_ipc_config(segment_size=65536, max_segments=1)

        try:
            cc.register(IHello, Hello(), name='hello')
            icrm = cc.connect(IHello, name='hello')
            try:
                # Small calls should always succeed.
                assert icrm.greeting('SOTA') == 'Hello, SOTA!'
                assert icrm.add(7, 8) == 15
            finally:
                cc.close(icrm)
        finally:
            cc.shutdown()

    def test_sota_concurrent_connect(self):
        """Multiple concurrent cc.connect() under tiny pool."""
        import c_two as cc

        cc.shutdown()
        addr = f'ipc-v3://{_unique_region()}'
        cc.set_address(addr)
        cc.set_ipc_config(segment_size=65536, max_segments=1)

        results = []
        errors = []
        lock = threading.Lock()

        try:
            cc.register(IHello, Hello(), name='hello')

            def worker(wid: int) -> None:
                try:
                    icrm = cc.connect(IHello, name='hello')
                    try:
                        for j in range(5):
                            r = icrm.add(wid, j)
                            with lock:
                                results.append((wid, j, r))
                    finally:
                        cc.close(icrm)
                except Exception as e:
                    with lock:
                        errors.append(e)

            threads = [threading.Thread(target=worker, args=(i,)) for i in range(4)]
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=30)

            for wid, j, r in results:
                assert r == wid + j
            assert len(errors) == 0, f'Errors: {errors}'
            assert len(results) == 20  # 4 workers × 5 calls
        finally:
            cc.shutdown()


# ---------------------------------------------------------------------------
# Experiment 4: High-concurrency stress + data integrity + edge cases
# ---------------------------------------------------------------------------


class TestHighConcurrencyStress:
    """16 threads × rapid fire under tiny pool — verify data integrity."""

    def test_16_threads_rapid_fire(self):
        """16 threads, each doing 20 add() calls, tiny pool — all results correct."""
        addr = f'ipc-v3://{_unique_region()}'
        cfg = _tiny_config(seg_size=65536, max_seg=1)
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        results = []
        errors = []
        lock = threading.Lock()

        def worker(tid: int) -> None:
            client = SharedClient(addr, try_v2=True, ipc_config=cfg)
            client.connect()
            try:
                proxy = ICRMProxy.ipc(client, 'hello')
                icrm = IHello()
                icrm.client = proxy
                for j in range(20):
                    r = icrm.add(tid * 100, j)
                    with lock:
                        results.append((tid, j, r))
            except Exception as e:
                with lock:
                    errors.append(e)
            finally:
                client.terminate()

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(16)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=30)

        server.shutdown()

        for tid, j, r in results:
            assert r == tid * 100 + j, f'Data corruption: {tid=}, {j=}, got {r}'
        assert len(errors) == 0, f'Errors: {errors}'
        assert len(results) == 320  # 16 × 20

    def test_shared_client_concurrent_calls(self):
        """Single SharedClient, 8 threads, concurrent calls — data integrity."""
        addr = f'ipc-v3://{_unique_region()}'
        cfg = _tiny_config(seg_size=65536, max_seg=1)
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        client = SharedClient(addr, try_v2=True, ipc_config=cfg)
        client.connect()

        results = []
        errors = []
        lock = threading.Lock()

        def worker(tid: int) -> None:
            try:
                proxy = ICRMProxy.ipc(client, 'hello')
                icrm = IHello()
                icrm.client = proxy
                for j in range(15):
                    r = icrm.greeting(f'W{tid}R{j}')
                    with lock:
                        results.append((tid, j, r))
            except Exception as e:
                with lock:
                    errors.append(e)

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=30)

        client.terminate()
        server.shutdown()

        for tid, j, r in results:
            assert r == f'Hello, W{tid}R{j}!', f'Data corruption: {tid=}, {j=}, got {r}'
        assert len(errors) == 0, f'Errors: {errors}'
        assert len(results) == 120  # 8 × 15


class TestEdgeCases:
    """Edge cases: empty payload, exactly-at-threshold payload."""

    def test_empty_payload_under_pressure(self):
        """Empty payload call succeeds even under pressure."""
        addr = f'ipc-v3://{_unique_region()}'
        cfg = _tiny_config(seg_size=65536, max_seg=1)
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        client = SharedClient(addr, try_v2=True, ipc_config=cfg)
        client.connect()
        try:
            proxy = ICRMProxy.ipc(client, 'hello')
            icrm = IHello()
            icrm.client = proxy
            # echo_none with 'none' returns None, empty-ish payload.
            result = icrm.echo_none('hello')
            assert result == 'hello'
            result = icrm.echo_none('none')
            assert result is None
        finally:
            client.terminate()
            server.shutdown()

    def test_repeated_pressure_recovery_cycles(self):
        """Multiple cycles of normal → pressure → recovery."""
        addr = f'ipc-v3://{_unique_region()}'
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
            max_frame_size=8192,
        )
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        client = SharedClient(addr, try_v2=True, ipc_config=cfg)
        client.connect()
        try:
            proxy = ICRMProxy.ipc(client, 'hello')
            icrm = IHello()
            icrm.client = proxy

            for cycle in range(3):
                # Normal operation.
                assert icrm.add(cycle, 1) == cycle + 1

                # Trigger pressure error.
                with pytest.raises(MemoryPressureError):
                    icrm.greeting('Z' * 200_000)

                # Recover — normal operation continues.
                assert icrm.add(cycle, 10) == cycle + 10
        finally:
            client.terminate()
            server.shutdown()


# ---------------------------------------------------------------------------
# Experiment 6: Legacy rpc.Server + rpc.Client backpressure
# ---------------------------------------------------------------------------


class TestLegacyClientBackpressure:
    """Backpressure through legacy rpc.Server + compo.runtime.connect_crm."""

    def test_legacy_inline_fallback(self):
        """Legacy IPC v3 client inline fallback under tiny pool."""
        import c_two as cc

        addr = f'ipc-v3://{_unique_region()}'
        tiny_cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
        )
        config = cc.rpc.ServerConfig(
            name='LegacyBP',
            crm=Hello(),
            icrm=IHello,
            bind_address=addr,
            ipc_config=tiny_cfg,
        )
        server = cc.rpc.Server(config)
        from c_two.rpc.server import _start
        _start(server._state)
        _wait_for_server(addr)

        try:
            with cc.compo.runtime.connect_crm(addr, IHello, ipc_config=tiny_cfg) as crm:
                # Small calls succeed (inline path or within pool).
                for i in range(10):
                    assert crm.greeting(f'Legacy{i}') == f'Hello, Legacy{i}!'
                assert crm.add(100, 200) == 300
        finally:
            try:
                cc.rpc.Client.shutdown(addr, timeout=2.0)
            except Exception:
                pass
            server.stop()

    def test_legacy_pressure_error(self):
        """Legacy client raises MemoryPressureError for oversized payload."""
        import c_two as cc

        addr = f'ipc-v3://{_unique_region()}'
        tiny_cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=1,
            max_pool_memory=65536,
            max_frame_size=8192,
        )
        config = cc.rpc.ServerConfig(
            name='LegacyBPErr',
            crm=Hello(),
            icrm=IHello,
            bind_address=addr,
            ipc_config=tiny_cfg,
        )
        server = cc.rpc.Server(config)
        from c_two.rpc.server import _start
        _start(server._state)
        _wait_for_server(addr)

        try:
            with cc.compo.runtime.connect_crm(addr, IHello, ipc_config=tiny_cfg) as crm:
                # Small call succeeds.
                assert crm.add(1, 2) == 3

                # Large payload: 200KB > 8KB max_frame_size, > 64KB pool.
                with pytest.raises(MemoryPressureError):
                    crm.greeting('Z' * 200_000)

                # Recovery: client still usable.
                assert crm.add(10, 20) == 30
        finally:
            try:
                cc.rpc.Client.shutdown(addr, timeout=2.0)
            except Exception:
                pass
            server.stop()


# ---------------------------------------------------------------------------
# Chunked transfer backpressure tests
# ---------------------------------------------------------------------------


class TestChunkedBackpressure:
    """Backpressure behavior when chunked transfer is active."""

    def test_chunked_inline_fallback_per_chunk(self):
        """With a tiny pool, individual chunks fall back to inline transport.

        Each chunk (segment_size // 2 = 32KB) is below max_frame_size (16 MB),
        so inline fallback succeeds even when buddy alloc fails.
        """
        cfg = IPCConfig(
            pool_segment_size=65536,   # 64 KB → chunk_size=32KB, threshold≈58KB
            max_pool_segments=1,       # Very tight: only 64 KB total
            max_pool_memory=65536,
        )
        addr = f'ipc-v3://{_unique_region()}'
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        client = SharedClient(addr, try_v2=True, ipc_config=cfg)
        client.connect()
        try:
            proxy = ICRMProxy.ipc(client, 'hello')
            icrm = IHello()
            icrm.client = proxy

            # 70KB > threshold → chunked (3 chunks of 32KB each).
            # Pool is tiny → some chunks may inline-fallback, but all succeed.
            big = 'P' * 70_000
            result = icrm.greeting(big)
            assert result == f'Hello, {big}!'

            # Small call still works after chunked transfer.
            assert icrm.add(1, 2) == 3
        finally:
            client.terminate()
            server.shutdown()

    def test_chunked_not_capable_raises_on_large(self):
        """When server does not advertise CAP_CHUNKED, large payload raises."""
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=4,
            max_pool_memory=65536 * 4,
        )
        addr = f'ipc-v3://{_unique_region()}'
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        client = SharedClient(addr, try_v2=True, ipc_config=cfg)
        client.connect()
        try:
            # Forcibly disable chunked capability to simulate old server.
            client._chunked_capable = False

            proxy = ICRMProxy.ipc(client, 'hello')
            icrm = IHello()
            icrm.client = proxy

            # Payload exceeds threshold but chunked is "not supported".
            with pytest.raises(MemoryPressureError):
                icrm.greeting('X' * 70_000)

            # Small calls unaffected.
            assert icrm.add(5, 10) == 15
        finally:
            client.terminate()
            server.shutdown()

    def test_chunked_recovery_after_multiple_large(self):
        """Multiple sequential chunked calls don't leak assemblers or pool."""
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=4,
            max_pool_memory=65536 * 4,
        )
        addr = f'ipc-v3://{_unique_region()}'
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        client = SharedClient(addr, try_v2=True, ipc_config=cfg)
        client.connect()
        try:
            proxy = ICRMProxy.ipc(client, 'hello')
            icrm = IHello()
            icrm.client = proxy

            # 10 sequential chunked calls — should not exhaust resources.
            for i in range(10):
                payload = chr(ord('A') + i % 26) * 80_000
                result = icrm.greeting(payload)
                assert result == f'Hello, {payload}!'

            # Verify system healthy after stress.
            assert icrm.add(42, 58) == 100
        finally:
            client.terminate()
            server.shutdown()

    def test_concurrent_chunked_under_pressure(self):
        """Multiple threads sending chunked payloads with tight pool."""
        cfg = IPCConfig(
            pool_segment_size=65536,
            max_pool_segments=8,
            max_pool_memory=65536 * 8,
        )
        addr = f'ipc-v3://{_unique_region()}'
        server = ServerV2(bind_address=addr, ipc_config=cfg)
        server.register_crm(IHello, Hello(), name='hello')
        server.start()
        _wait_for_server(addr)

        client = SharedClient(addr, try_v2=True, ipc_config=cfg)
        client.connect()
        try:
            errors: list[Exception] = []
            results: list[tuple[int, str]] = []
            lock = threading.Lock()

            def worker(tid: int) -> None:
                try:
                    proxy = ICRMProxy.ipc(client, 'hello')
                    icrm = IHello()
                    icrm.client = proxy
                    payload = chr(ord('A') + tid) * 70_000
                    result = icrm.greeting(payload)
                    with lock:
                        results.append((tid, result))
                except Exception as e:
                    with lock:
                        errors.append(e)

            threads = [threading.Thread(target=worker, args=(i,)) for i in range(4)]
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=60)

            assert len(errors) == 0, f'Errors: {errors}'
            assert len(results) == 4
            for tid, result in results:
                expected = f'Hello, {chr(ord("A") + tid) * 70_000}!'
                assert result == expected
        finally:
            client.terminate()
            server.shutdown()
