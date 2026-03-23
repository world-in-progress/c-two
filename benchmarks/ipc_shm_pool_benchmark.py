"""
IPC v2 SHM Pool Benchmark — compare pool vs per-request SHM performance.

Measures round-trip latency and throughput for echo RPCs across a range of
payload sizes with pool_enabled=True vs pool_enabled=False.

Usage:
    uv run python benchmarks/ipc_shm_pool_benchmark.py
"""

import statistics
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / 'src'))

import c_two as cc                                        # noqa: E402
from c_two.rpc import Server, ServerConfig                # noqa: E402
from c_two.rpc.server import _start                       # noqa: E402
from c_two.rpc.ipc.ipc_server import IPCConfig            # noqa: E402

# ---------------------------------------------------------------------------
# Benchmark CRM — simple echo
# ---------------------------------------------------------------------------

@cc.icrm(namespace='ipcv2.bench', version='0.1.0')
class IEcho:
    def echo(self, data: bytes) -> bytes: ...


class Echo:
    def echo(self, data: bytes) -> bytes:
        return data


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _start_server(address: str, ipc_config: IPCConfig) -> Server:
    server = Server(ServerConfig(
        name='BenchServer',
        crm=Echo(),
        icrm=IEcho,
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


def _shutdown(address: str, server: Server) -> None:
    try:
        cc.rpc.Client.shutdown(address, timeout=2.0)
    except Exception:
        pass
    time.sleep(0.1)
    try:
        server.stop()
    except Exception:
        pass


def _fmt_size(n: int) -> str:
    if n >= 1_048_576:
        return f'{n / 1_048_576:.0f} MB'
    if n >= 1024:
        return f'{n / 1024:.0f} KB'
    return f'{n} B'


# ---------------------------------------------------------------------------
# Core benchmark
# ---------------------------------------------------------------------------

def run_benchmark(
    payload_size: int,
    n_warmup: int,
    n_rounds: int,
    pool_enabled: bool,
    pool_segment_size: int = 256 * 1024 * 1024,
) -> dict:
    """Run echo benchmark, returning latency/throughput stats."""
    address = f'ipc-v2://bench_{uuid.uuid4().hex[:8]}'
    ipc_config = IPCConfig(
        pool_enabled=pool_enabled,
        pool_segment_size=pool_segment_size,
    )

    server = _start_server(address, ipc_config)
    payload = b'\xAB' * payload_size
    latencies: list[float] = []

    try:
        with cc.compo.runtime.connect_crm(address, IEcho, ipc_config=ipc_config) as crm:
            # Warmup
            for _ in range(n_warmup):
                crm.echo(payload)

            # Timed rounds
            for _ in range(n_rounds):
                t0 = time.perf_counter()
                result = crm.echo(payload)
                latencies.append(time.perf_counter() - t0)

            assert len(result) == payload_size
    finally:
        _shutdown(address, server)

    total = sum(latencies)
    ops = n_rounds / total
    throughput_mbs = (payload_size * 2 * ops) / (1024 * 1024)  # round-trip bytes

    return {
        'pool': pool_enabled,
        'size': payload_size,
        'rounds': n_rounds,
        'ops_sec': ops,
        'throughput_mbs': throughput_mbs,
        'p50_ms': statistics.median(latencies) * 1000,
        'p95_ms': sorted(latencies)[int(n_rounds * 0.95)] * 1000,
        'min_ms': min(latencies) * 1000,
        'max_ms': max(latencies) * 1000,
        'avg_ms': statistics.mean(latencies) * 1000,
    }


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

PAYLOAD_SIZES = [
    512,                    # 512 B  — inline path (below 1MB threshold)
    1 * 1024,              # 1 KB   — inline path
    64 * 1024,             # 64 KB  — inline path
    1 * 1024 * 1024,       # 1 MB   — SHM threshold boundary
    2 * 1024 * 1024,       # 2 MB   — SHM path
    10 * 1024 * 1024,      # 10 MB  — SHM path
    50 * 1024 * 1024,      # 50 MB  — SHM path
    100 * 1024 * 1024,     # 100 MB — SHM path
]

# Fewer rounds for large payloads
def _rounds(size: int) -> tuple[int, int]:
    if size >= 50 * 1024 * 1024:
        return 3, 10
    if size >= 10 * 1024 * 1024:
        return 5, 20
    if size >= 1 * 1024 * 1024:
        return 10, 50
    return 20, 200


def main() -> None:
    print('=' * 90)
    print('IPC v2 SHM Pool Benchmark — pool vs per-request SHM')
    print('=' * 90)
    print()

    header = (
        f'{"Payload":>10s}  {"Mode":>12s}  '
        f'{"ops/s":>8s}  {"MB/s":>8s}  '
        f'{"avg":>8s}  {"p50":>8s}  {"p95":>8s}  '
        f'{"min":>8s}  {"max":>8s}  {"Δ avg":>8s}'
    )
    print(header)
    print('-' * len(header))

    for size in PAYLOAD_SIZES:
        n_warmup, n_rounds = _rounds(size)
        results = {}

        for pool in (True, False):
            r = run_benchmark(size, n_warmup, n_rounds, pool_enabled=pool)
            results[pool] = r

        # Print results with comparison
        pool_r = results[True]
        npool_r = results[False]
        speedup = npool_r['avg_ms'] / pool_r['avg_ms'] if pool_r['avg_ms'] > 0 else 0

        for pool, r in [(True, pool_r), (False, npool_r)]:
            mode = 'pool' if pool else 'per-request'
            delta = ''
            if not pool:
                if speedup > 1:
                    delta = f'{speedup:.2f}x ▲'
                else:
                    delta = f'{1/speedup:.2f}x ▼'

            print(
                f'{_fmt_size(r["size"]):>10s}  {mode:>12s}  '
                f'{r["ops_sec"]:>8.1f}  {r["throughput_mbs"]:>8.1f}  '
                f'{r["avg_ms"]:>7.3f}  {r["p50_ms"]:>7.3f}  {r["p95_ms"]:>7.3f}  '
                f'{r["min_ms"]:>7.3f}  {r["max_ms"]:>7.3f}  {delta:>8s}'
            )
        print()

    print('=' * 90)
    print('Notes:')
    print('  - "pool" = pre-allocated SHM reused across calls (Phase 1 optimization)')
    print('  - "per-request" = shm_open/ftruncate/mmap/munmap/shm_unlink per call')
    print('  - Payloads < 1 MB use inline transport (no SHM), so pool has no effect')
    print('  - MB/s = round-trip throughput (payload × 2 × ops/s)')
    print('  - Δ avg = pool speedup vs per-request (> 1x = pool is faster)')


if __name__ == '__main__':
    main()
