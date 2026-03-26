"""
Integration tests for IPC v2 transport (UDS control plane + SharedMemory data plane).
"""

import struct
import threading
import time
import uuid

import pytest

import c_two as cc
from c_two.rpc import ConcurrencyConfig, ConcurrencyMode, Server, ServerConfig
from c_two.rpc.server import _start
from c_two.rpc.ipc.ipc_protocol import IPCConfig

from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello


@pytest.fixture
def ipc_address():
    return f'ipc-v2://ipc_test_{uuid.uuid4().hex[:8]}'


def _start_server(address, concurrency=None, ipc_config=None):
    kwargs = dict(
        name='TestIPC',
        crm=Hello(),
        icrm=IHello,
        bind_address=address,
    )
    if concurrency:
        kwargs['concurrency'] = concurrency
    if ipc_config:
        kwargs['ipc_config'] = ipc_config

    server = Server(ServerConfig(**kwargs))
    _start(server._state)

    for _ in range(50):
        try:
            if cc.rpc.Client.ping(address, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.1)
    return server


def _shutdown(address, server):
    try:
        cc.rpc.Client.shutdown(address, timeout=2.0)
    except Exception:
        pass
    time.sleep(0.1)
    try:
        server.stop()
    except Exception:
        pass


class TestIPCv2Basic:
    """Basic connectivity and RPC tests."""

    def test_ping(self, ipc_address):
        server = _start_server(ipc_address)
        try:
            assert cc.rpc.Client.ping(ipc_address, timeout=1.0) is True
        finally:
            _shutdown(ipc_address, server)

    def test_shutdown(self, ipc_address):
        server = _start_server(ipc_address)
        result = cc.rpc.Client.shutdown(ipc_address, timeout=2.0)
        assert result is True
        time.sleep(0.2)
        assert cc.rpc.Client.ping(ipc_address, timeout=0.5) is False

    def test_crm_call_inline(self, ipc_address):
        """Small payload — should use inline transport."""
        server = _start_server(ipc_address)
        try:
            with cc.compo.runtime.connect_crm(ipc_address, IHello) as crm:
                result = crm.greeting('IPC')
            assert result == 'Hello, IPC!'
        finally:
            _shutdown(ipc_address, server)

    def test_crm_call_multiple(self, ipc_address):
        """Multiple sequential calls."""
        server = _start_server(ipc_address)
        try:
            for i in range(5):
                with cc.compo.runtime.connect_crm(ipc_address, IHello) as crm:
                    result = crm.greeting(f'call_{i}')
                assert result == f'Hello, call_{i}!'
        finally:
            _shutdown(ipc_address, server)


class TestIPCv2SharedMemory:
    """Tests for SharedMemory data plane with large payloads."""

    def test_large_response(self, ipc_address):
        """CRM returns data larger than SHM threshold — should trigger SHM path."""
        server = _start_server(ipc_address)
        try:
            with cc.compo.runtime.connect_crm(ipc_address, IHello) as crm:
                result = crm.greeting('world')
            assert result == 'Hello, world!'
        finally:
            _shutdown(ipc_address, server)

    def test_pool_shm_path(self, ipc_address):
        """Lower SHM threshold to force pool SHM usage for normal payloads."""
        low_threshold_config = IPCConfig(shm_threshold=16, pool_segment_size=4 * 1024 * 1024)
        server = _start_server(ipc_address, ipc_config=low_threshold_config)
        try:
            with cc.compo.runtime.connect_crm(ipc_address, IHello, ipc_config=low_threshold_config) as crm:
                result = crm.greeting('pool')
            assert result == 'Hello, pool!'
        finally:
            _shutdown(ipc_address, server)

    def test_pool_shm_multiple_calls(self, ipc_address):
        """Multiple calls reusing pool SHM segments."""
        low_threshold_config = IPCConfig(shm_threshold=16, pool_segment_size=4 * 1024 * 1024)
        server = _start_server(ipc_address, ipc_config=low_threshold_config)
        try:
            for i in range(10):
                with cc.compo.runtime.connect_crm(ipc_address, IHello, ipc_config=low_threshold_config) as crm:
                    result = crm.greeting(f'pool_{i}')
                assert result == f'Hello, pool_{i}!'
        finally:
            _shutdown(ipc_address, server)

    def test_pool_disabled_fallback(self, ipc_address):
        """With pool disabled, should fall back to per-request SHM."""
        no_pool_config = IPCConfig(shm_threshold=16, pool_enabled=False)
        server = _start_server(ipc_address, ipc_config=no_pool_config)
        try:
            with cc.compo.runtime.connect_crm(ipc_address, IHello, ipc_config=no_pool_config) as crm:
                result = crm.greeting('fallback')
            assert result == 'Hello, fallback!'
        finally:
            _shutdown(ipc_address, server)


class TestIPCv2Concurrency:
    """Concurrent access tests on ipc-v2://."""

    def test_concurrent_reads(self, ipc_address):
        server = _start_server(
            ipc_address,
            concurrency=ConcurrencyConfig(mode=ConcurrencyMode.READ_PARALLEL, max_workers=4),
        )
        try:
            results = []
            errors = []
            barrier = threading.Barrier(4)

            def worker(name):
                barrier.wait()
                try:
                    with cc.compo.runtime.connect_crm(ipc_address, IHello) as crm:
                        result = crm.greeting(name)
                    results.append(result)
                except Exception as exc:
                    errors.append(exc)

            threads = [threading.Thread(target=worker, args=(f't{i}',)) for i in range(4)]
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=10)

            assert len(errors) == 0, f'Errors: {errors}'
            assert len(results) == 4
            for i in range(4):
                assert f'Hello, t{i}!' in results
        finally:
            _shutdown(ipc_address, server)

    def test_exclusive_mode(self, ipc_address):
        server = _start_server(
            ipc_address,
            concurrency=ConcurrencyConfig(mode=ConcurrencyMode.EXCLUSIVE),
        )
        try:
            with cc.compo.runtime.connect_crm(ipc_address, IHello) as crm:
                result = crm.greeting('exclusive')
            assert result == 'Hello, exclusive!'
        finally:
            _shutdown(ipc_address, server)


class TestIPCv2EdgeCases:
    """Edge cases and error handling."""

    def test_ping_nonexistent(self):
        assert cc.rpc.Client.ping('ipc-v2://nonexistent_server_xyz', timeout=0.3) is False

    def test_shutdown_nonexistent(self):
        assert cc.rpc.Client.shutdown('ipc-v2://nonexistent_server_xyz', timeout=0.3) is True


class TestIPCv2PoolDecay:
    """Pool SHM idle decay and re-handshake tests."""

    def test_pool_decay_and_rehandshake(self, ipc_address):
        """Pool should be torn down after idle timeout, rebuilt on demand."""
        decay_config = IPCConfig(
            shm_threshold=16,
            pool_segment_size=4 * 1024 * 1024,
            pool_decay_seconds=0.5,
        )
        server = _start_server(ipc_address, ipc_config=decay_config)
        try:
            # First call — establishes pool
            with cc.compo.runtime.connect_crm(ipc_address, IHello, ipc_config=decay_config) as crm:
                assert crm.greeting('before') == 'Hello, before!'

            # Wait beyond decay timeout
            time.sleep(0.8)

            # Second call — pool decayed, triggers re-handshake
            with cc.compo.runtime.connect_crm(ipc_address, IHello, ipc_config=decay_config) as crm:
                assert crm.greeting('after') == 'Hello, after!'
        finally:
            _shutdown(ipc_address, server)

    def test_pool_decay_mid_session(self, ipc_address):
        """Pool decay during a persistent connection with multiple calls."""
        decay_config = IPCConfig(
            shm_threshold=16,
            pool_segment_size=4 * 1024 * 1024,
            pool_decay_seconds=0.3,
        )
        server = _start_server(ipc_address, ipc_config=decay_config)
        try:
            # 3 calls, with decay window between 2nd and 3rd
            with cc.compo.runtime.connect_crm(ipc_address, IHello, ipc_config=decay_config) as crm:
                assert crm.greeting('c1') == 'Hello, c1!'
                assert crm.greeting('c2') == 'Hello, c2!'
                time.sleep(0.5)  # trigger decay on next call
                assert crm.greeting('c3') == 'Hello, c3!'
        finally:
            _shutdown(ipc_address, server)

    def test_pool_decay_disabled(self, ipc_address):
        """pool_decay_seconds=0 should keep pool alive indefinitely."""
        no_decay_config = IPCConfig(
            shm_threshold=16,
            pool_segment_size=4 * 1024 * 1024,
            pool_decay_seconds=0,
        )
        server = _start_server(ipc_address, ipc_config=no_decay_config)
        try:
            with cc.compo.runtime.connect_crm(ipc_address, IHello, ipc_config=no_decay_config) as crm:
                assert crm.greeting('a') == 'Hello, a!'
                time.sleep(0.5)
                # Should still work without re-handshake (no decay)
                assert crm.greeting('b') == 'Hello, b!'
        finally:
            _shutdown(ipc_address, server)

    def test_unified_pool_single_shm(self, ipc_address):
        """Unified bidirectional pool uses a single SHM per connection."""
        config = IPCConfig(shm_threshold=16, pool_segment_size=4 * 1024 * 1024)
        server = _start_server(ipc_address, ipc_config=config)
        try:
            with cc.compo.runtime.connect_crm(ipc_address, IHello, ipc_config=config) as crm:
                assert crm.greeting('unified') == 'Hello, unified!'
        finally:
            _shutdown(ipc_address, server)


class TestIPCv2PoolEnhancements:
    """P0 SHM pool enhancements: segment chain expansion, heartbeat, decay, max segments.

    Server uses a generous ``pool_segment_size`` so it does not clamp client
    expansion requests.  The client uses a small, page-aligned segment size
    (16 KB on ARM64 macOS) so oversized payloads trigger real expansion.
    ``echo_none`` is used because its response wire-size is always smaller
    than the request wire-size, avoiding segment-overflow on the reply path.
    """

    _PAGE = 16384  # ARM64 macOS page size
    _SERVER_SEG = 256 * 1024  # 256 KB — generous upper bound

    def test_segment_chain_expansion(self, ipc_address):
        """Payload exceeding initial pool segment triggers chain expansion; RPCs succeed."""
        server_cfg = IPCConfig(shm_threshold=16, pool_segment_size=self._SERVER_SEG)
        client_cfg = IPCConfig(
            shm_threshold=16, pool_segment_size=self._PAGE, max_pool_segments=4,
        )
        server = _start_server(ipc_address, ipc_config=server_cfg)
        try:
            with cc.compo.runtime.connect_crm(
                ipc_address, IHello, ipc_config=client_cfg,
            ) as crm:
                # Small payload — fits in segment[0] (16 KB)
                assert crm.echo_none('small') == 'small'

                # ~20 KB — exceeds segment[0], triggers expansion to segment[1]
                big = 'A' * 20_000
                assert crm.echo_none(big) == big

                # ~50 KB — exceeds segment[1], triggers expansion to segment[2]
                bigger = 'B' * 50_000
                assert crm.echo_none(bigger) == bigger
        finally:
            _shutdown(ipc_address, server)

    def test_heartbeat_keeps_connection(self, ipc_address):
        """Connection survives idle period when heartbeat is active."""
        config = IPCConfig(heartbeat_interval=1.0, heartbeat_timeout=3.0)
        server = _start_server(ipc_address, ipc_config=config)
        try:
            with cc.compo.runtime.connect_crm(
                ipc_address, IHello, ipc_config=config,
            ) as crm:
                assert crm.greeting('before') == 'Hello, before!'
                # Idle longer than heartbeat_interval but shorter than timeout
                time.sleep(2.0)
                assert crm.greeting('after') == 'Hello, after!'
        finally:
            _shutdown(ipc_address, server)

    def test_pool_decay_tail_segments(self, ipc_address):
        """Tail segments decay after idle; re-expansion works on demand."""
        server_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._SERVER_SEG,
            pool_decay_seconds=1.0,
        )
        client_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._PAGE,
            pool_decay_seconds=1.0,
            max_pool_segments=4,
        )
        server = _start_server(ipc_address, ipc_config=server_cfg)
        try:
            with cc.compo.runtime.connect_crm(
                ipc_address, IHello, ipc_config=client_cfg,
            ) as crm:
                # Trigger segment expansion with large payload
                big = 'A' * 20_000
                assert crm.echo_none(big) == big

                # Exceed decay threshold
                time.sleep(1.5)

                # Small call — triggers decay check; tail segments should be reclaimed
                assert crm.echo_none('tiny') == 'tiny'

                # Large call again — re-expands if segments were decayed
                assert crm.echo_none(big) == big
        finally:
            _shutdown(ipc_address, server)

    def test_max_pool_segments_respected(self, ipc_address):
        """Exceeding all pool segments falls back to per-request SHM, not crash."""
        server_cfg = IPCConfig(shm_threshold=16, pool_segment_size=self._SERVER_SEG)
        client_cfg = IPCConfig(
            shm_threshold=16, pool_segment_size=self._PAGE, max_pool_segments=2,
        )
        server = _start_server(ipc_address, ipc_config=server_cfg)
        try:
            with cc.compo.runtime.connect_crm(
                ipc_address, IHello, ipc_config=client_cfg,
            ) as crm:
                # Fits in segment[0] (16 KB)
                assert crm.echo_none('hi') == 'hi'

                # Exceeds segment[0] → triggers expansion to segment[1]
                mid = 'M' * 20_000
                assert crm.echo_none(mid) == mid

                # Exceeds both segments; max_pool_segments=2 → per-request SHM fallback
                huge = 'H' * 50_000
                assert crm.echo_none(huge) == huge
        finally:
            _shutdown(ipc_address, server)


# ---------------------------------------------------------------------------
# Bytes echo CRM for precise elastic expansion testing
# ---------------------------------------------------------------------------

@cc.icrm(namespace='test.echo_bytes', version='0.1.0')
class IEchoBytes:
    def echo(self, data: bytes) -> bytes:
        ...


class EchoBytes:
    def echo(self, data: bytes) -> bytes:
        return data


def _start_echo_server(address, ipc_config):
    server = Server(ServerConfig(
        name='EchoElastic',
        crm=EchoBytes(),
        icrm=IEchoBytes,
        bind_address=address,
        ipc_config=ipc_config,
    ))
    _start(server._state)
    for _ in range(50):
        try:
            if cc.rpc.Client.ping(address, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.1)
    return server


class TestSegmentChainElasticity:
    """Elastic expansion end-to-end with bytes echo.

    Uses small segment sizes (64 KB) to trigger expansion without
    allocating hundreds of megabytes in tests.  Both request and
    response wire data must fit in the selected segment for pool path,
    so the echo CRM is ideal: response is the same size as request data.
    """

    _SEG_SIZE = 64 * 1024       # 64 KB initial segment
    _SERVER_SEG = 512 * 1024    # 512 KB — server never clamps expansion

    @pytest.fixture
    def echo_addr(self):
        return f'ipc-v2://echo_{uuid.uuid4().hex[:8]}'

    def test_small_payload_uses_initial_segment(self, echo_addr):
        """Payload fitting the initial segment does not trigger expansion."""
        srv_cfg = IPCConfig(shm_threshold=16, pool_segment_size=self._SERVER_SEG)
        cli_cfg = IPCConfig(
            shm_threshold=16, pool_segment_size=self._SEG_SIZE, max_pool_segments=4,
        )
        server = _start_echo_server(echo_addr, srv_cfg)
        try:
            with cc.compo.runtime.connect_crm(echo_addr, IEchoBytes, ipc_config=cli_cfg) as crm:
                payload = b'\xAA' * 1024  # 1 KB — easily fits in 64 KB
                result = crm.echo(payload)
                assert result == payload
        finally:
            _shutdown(echo_addr, server)

    def test_expansion_triggered_and_data_preserved(self, echo_addr):
        """Payload exceeding initial segment triggers expansion; data round-trips intact."""
        srv_cfg = IPCConfig(shm_threshold=16, pool_segment_size=self._SERVER_SEG)
        cli_cfg = IPCConfig(
            shm_threshold=16, pool_segment_size=self._SEG_SIZE, max_pool_segments=4,
        )
        server = _start_echo_server(echo_addr, srv_cfg)
        try:
            with cc.compo.runtime.connect_crm(echo_addr, IEchoBytes, ipc_config=cli_cfg) as crm:
                # 100 KB payload — exceeds 64 KB segment → triggers expansion
                payload = bytes(range(256)) * 400  # 100 KB, non-trivial pattern
                result = crm.echo(payload)
                assert result == payload
        finally:
            _shutdown(echo_addr, server)

    def test_segment_reuse_after_expansion(self, echo_addr):
        """After expanding to segment[1], a small payload still uses segment[0]."""
        srv_cfg = IPCConfig(shm_threshold=16, pool_segment_size=self._SERVER_SEG)
        cli_cfg = IPCConfig(
            shm_threshold=16, pool_segment_size=self._SEG_SIZE, max_pool_segments=4,
        )
        server = _start_echo_server(echo_addr, srv_cfg)
        try:
            with cc.compo.runtime.connect_crm(echo_addr, IEchoBytes, ipc_config=cli_cfg) as crm:
                # 1. Small (fits in segment[0])
                small = b'\x01' * 1024
                assert crm.echo(small) == small

                # 2. Large (triggers expansion to segment[1])
                large = b'\x02' * (100 * 1024)
                assert crm.echo(large) == large

                # 3. Small again (should still work — reuses segment[0])
                assert crm.echo(small) == small
        finally:
            _shutdown(echo_addr, server)

    def test_progressive_expansion_three_segments(self, echo_addr):
        """Progressively larger payloads create a 3-segment chain."""
        srv_cfg = IPCConfig(shm_threshold=16, pool_segment_size=self._SERVER_SEG)
        cli_cfg = IPCConfig(
            shm_threshold=16, pool_segment_size=self._SEG_SIZE, max_pool_segments=4,
        )
        server = _start_echo_server(echo_addr, srv_cfg)
        try:
            with cc.compo.runtime.connect_crm(echo_addr, IEchoBytes, ipc_config=cli_cfg) as crm:
                # Segment[0] = 64 KB
                p1 = b'\x10' * 1024          # 1 KB → fits [0]
                assert crm.echo(p1) == p1

                # Segment[1] ≈ 100 KB (max(64KB, wire_size))
                p2 = b'\x20' * (80 * 1024)   # 80 KB → triggers expansion
                assert crm.echo(p2) == p2

                # Segment[2] ≈ 200 KB
                p3 = b'\x30' * (150 * 1024)  # 150 KB → triggers second expansion
                assert crm.echo(p3) == p3

                # All sizes still work (each uses its segment)
                assert crm.echo(p1) == p1
                assert crm.echo(p2) == p2
                assert crm.echo(p3) == p3
        finally:
            _shutdown(echo_addr, server)

    def test_fallback_after_max_segments_exhausted(self, echo_addr):
        """After max_pool_segments reached, oversized payload falls back to per-request SHM."""
        srv_cfg = IPCConfig(shm_threshold=16, pool_segment_size=self._SERVER_SEG)
        cli_cfg = IPCConfig(
            shm_threshold=16, pool_segment_size=self._SEG_SIZE, max_pool_segments=2,
        )
        server = _start_echo_server(echo_addr, srv_cfg)
        try:
            with cc.compo.runtime.connect_crm(echo_addr, IEchoBytes, ipc_config=cli_cfg) as crm:
                # Fill 2 segments
                assert crm.echo(b'\x01' * 1024) == b'\x01' * 1024              # seg[0]
                assert crm.echo(b'\x02' * (80 * 1024)) == b'\x02' * (80 * 1024)  # seg[1]

                # This exceeds both segments; max_pool_segments=2 → per-request SHM
                huge = b'\x03' * (200 * 1024)
                assert crm.echo(huge) == huge  # must succeed, not crash
        finally:
            _shutdown(echo_addr, server)

    def test_response_integrity_after_expansion(self, echo_addr):
        """Server reply uses expanded segment — verifies reply() segment selection works."""
        srv_cfg = IPCConfig(shm_threshold=16, pool_segment_size=self._SERVER_SEG)
        cli_cfg = IPCConfig(
            shm_threshold=16, pool_segment_size=self._SEG_SIZE, max_pool_segments=4,
        )
        server = _start_echo_server(echo_addr, srv_cfg)
        try:
            with cc.compo.runtime.connect_crm(echo_addr, IEchoBytes, ipc_config=cli_cfg) as crm:
                # Send 100 KB — both request and response use expanded segment
                payload = bytes(range(256)) * 400
                for _ in range(10):  # repeat to confirm stability
                    assert crm.echo(payload) == payload
        finally:
            _shutdown(echo_addr, server)


# ---------------------------------------------------------------------------
# Generator CRM for asymmetric (small-request → large-response) testing
# ---------------------------------------------------------------------------

@cc.icrm(namespace='test.generator', version='0.1.0')
class IGenerator:
    def generate(self, size: int) -> bytes:
        ...


class Generator:
    def generate(self, size: int) -> bytes:
        return b'\xBB' * size


def _start_generator_server(address, ipc_config):
    server = Server(ServerConfig(
        name='Generator',
        crm=Generator(),
        icrm=IGenerator,
        bind_address=address,
        ipc_config=ipc_config,
    ))
    _start(server._state)
    for _ in range(50):
        try:
            if cc.rpc.Client.ping(address, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.1)
    return server


class TestSplitPool:
    """Split pool architecture: separate client outbound + server response pools.

    Uses small segment sizes (64 KB) so server auto-expansion is triggered
    without consuming hundreds of MB in test runs.

    Key scenarios:
    - Asymmetric: small request → large response (server response pool expands)
    - Symmetric echo: both pools exercise normally
    - Budget exhaustion: large responses exceed budget → per-request SHM fallback
    - Progressive expansion: increasingly large responses expand response pool
    """

    _SEG_SIZE = 64 * 1024       # 64 KB — small enough to trigger expansion
    _SERVER_SEG = 64 * 1024     # Same — server starts small too
    _MAX_SEGS = 4

    @pytest.fixture
    def gen_addr(self):
        return f'ipc-v2://gen_{uuid.uuid4().hex[:8]}'

    @pytest.fixture
    def echo_addr(self):
        return f'ipc-v2://spe_{uuid.uuid4().hex[:8]}'

    def test_small_request_large_response(self, gen_addr):
        """Small int request → large bytes response. Server response pool must expand."""
        srv_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._SERVER_SEG,
            max_pool_segments=self._MAX_SEGS,
        )
        cli_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._SEG_SIZE,
            max_pool_segments=self._MAX_SEGS,
        )
        server = _start_generator_server(gen_addr, srv_cfg)
        try:
            with cc.compo.runtime.connect_crm(
                gen_addr, IGenerator, ipc_config=cli_cfg,
            ) as crm:
                # 100 KB response exceeds server's initial 64 KB segment
                result = crm.generate(100 * 1024)
                assert len(result) == 100 * 1024
                assert result == b'\xBB' * (100 * 1024)
        finally:
            _shutdown(gen_addr, server)

    def test_progressive_response_expansion(self, gen_addr):
        """Progressively larger responses force the server to add response segments."""
        srv_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._SERVER_SEG,
            max_pool_segments=self._MAX_SEGS,
        )
        cli_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._SEG_SIZE,
            max_pool_segments=self._MAX_SEGS,
        )
        server = _start_generator_server(gen_addr, srv_cfg)
        try:
            with cc.compo.runtime.connect_crm(
                gen_addr, IGenerator, ipc_config=cli_cfg,
            ) as crm:
                # Seg[0] = 64 KB — fits
                r1 = crm.generate(1024)
                assert len(r1) == 1024

                # Exceeds seg[0] → server creates seg[1]
                r2 = crm.generate(80 * 1024)
                assert len(r2) == 80 * 1024

                # Exceeds seg[1] → server creates seg[2]
                r3 = crm.generate(150 * 1024)
                assert len(r3) == 150 * 1024

                # All sizes still work (server reuses appropriate segments)
                assert len(crm.generate(1024)) == 1024
                assert len(crm.generate(80 * 1024)) == 80 * 1024
                assert len(crm.generate(150 * 1024)) == 150 * 1024
        finally:
            _shutdown(gen_addr, server)

    def test_symmetric_echo_through_split_pools(self, echo_addr):
        """Echo CRM exercises both outbound (request) and response pools."""
        srv_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._SERVER_SEG,
            max_pool_segments=self._MAX_SEGS,
        )
        cli_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._SEG_SIZE,
            max_pool_segments=self._MAX_SEGS,
        )
        server = _start_echo_server(echo_addr, srv_cfg)
        try:
            with cc.compo.runtime.connect_crm(
                echo_addr, IEchoBytes, ipc_config=cli_cfg,
            ) as crm:
                # Request and response same size — both pools must expand
                payload = b'\xCC' * (100 * 1024)
                assert crm.echo(payload) == payload
        finally:
            _shutdown(echo_addr, server)

    def test_server_budget_exhaustion_fallback(self, gen_addr):
        """When server response pool budget is exhausted, falls back to per-request SHM."""
        # Server has tight budget: 2 segments × 64 KB = 128 KB
        srv_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._SERVER_SEG,
            max_pool_segments=2,
            max_pool_memory=2 * self._SERVER_SEG,
        )
        cli_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._SEG_SIZE,
            max_pool_segments=self._MAX_SEGS,
        )
        server = _start_generator_server(gen_addr, srv_cfg)
        try:
            with cc.compo.runtime.connect_crm(
                gen_addr, IGenerator, ipc_config=cli_cfg,
            ) as crm:
                # Fill 2 server segments
                assert len(crm.generate(1024)) == 1024         # seg[0]
                assert len(crm.generate(80 * 1024)) == 80 * 1024  # seg[1]

                # Exceeds both segments + budget → per-request SHM (must succeed)
                big = crm.generate(200 * 1024)
                assert len(big) == 200 * 1024
        finally:
            _shutdown(gen_addr, server)

    def test_client_outbound_expansion_independent(self, echo_addr):
        """Client outbound pool expansion is independent of server response pool."""
        # Server starts big, client starts small
        srv_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=256 * 1024,
            max_pool_segments=self._MAX_SEGS,
        )
        cli_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._SEG_SIZE,
            max_pool_segments=self._MAX_SEGS,
        )
        server = _start_echo_server(echo_addr, srv_cfg)
        try:
            with cc.compo.runtime.connect_crm(
                echo_addr, IEchoBytes, ipc_config=cli_cfg,
            ) as crm:
                # Client expands outbound pool; server already has enough response space
                payload = b'\xDD' * (100 * 1024)
                result = crm.echo(payload)
                assert result == payload

                # Even bigger — client expands again
                payload2 = b'\xEE' * (200 * 1024)
                result2 = crm.echo(payload2)
                assert result2 == payload2
        finally:
            _shutdown(echo_addr, server)

    def test_repeated_asymmetric_calls_stable(self, gen_addr):
        """Repeated asymmetric calls are stable (no leaks or state corruption)."""
        srv_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._SERVER_SEG,
            max_pool_segments=self._MAX_SEGS,
        )
        cli_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._SEG_SIZE,
            max_pool_segments=self._MAX_SEGS,
        )
        server = _start_generator_server(gen_addr, srv_cfg)
        try:
            with cc.compo.runtime.connect_crm(
                gen_addr, IGenerator, ipc_config=cli_cfg,
            ) as crm:
                for _ in range(20):
                    r = crm.generate(100 * 1024)
                    assert len(r) == 100 * 1024
        finally:
            _shutdown(gen_addr, server)

    def test_mixed_sizes_alternating(self, gen_addr):
        """Alternating small and large responses reuse appropriate segments."""
        srv_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._SERVER_SEG,
            max_pool_segments=self._MAX_SEGS,
        )
        cli_cfg = IPCConfig(
            shm_threshold=16,
            pool_segment_size=self._SEG_SIZE,
            max_pool_segments=self._MAX_SEGS,
        )
        server = _start_generator_server(gen_addr, srv_cfg)
        try:
            with cc.compo.runtime.connect_crm(
                gen_addr, IGenerator, ipc_config=cli_cfg,
            ) as crm:
                sizes = [1024, 80 * 1024, 512, 150 * 1024, 2048, 80 * 1024]
                for size in sizes:
                    r = crm.generate(size)
                    assert len(r) == size
        finally:
            _shutdown(gen_addr, server)
