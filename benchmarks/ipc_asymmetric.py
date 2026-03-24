"""
IPC v2 Asymmetric Benchmark — small request → large response.

Tests the split pool architecture where the server response pool
must auto-expand independently of client outbound pool.

Usage:
    uv run python benchmarks/ipc_asymmetric.py
"""

import json
import statistics
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / 'src'))

import c_two as cc                                          # noqa: E402
from c_two.rpc import Server, ServerConfig                  # noqa: E402
from c_two.rpc.server import _start                         # noqa: E402
from c_two.rpc.ipc.ipc_protocol import IPCConfig            # noqa: E402

# ---------------------------------------------------------------------------
# Asymmetric CRM — small int request → large bytes response
# ---------------------------------------------------------------------------

@cc.icrm(namespace='ipcv2.asym', version='0.1.0')
class IGenerator:
    def generate(self, size: int) -> bytes: ...


class Generator:
    """Returns `size` bytes of 0xBB."""
    def generate(self, size: int) -> bytes:
        return b'\xBB' * size


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _start_server(address: str, ipc_config: IPCConfig) -> Server:
    server = Server(ServerConfig(
        name='AsymBench',
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
    if n >= 1024 * 1024 * 1024:
        return f'{n / (1024**3):.0f} GB'
    if n >= 1024 * 1024:
        return f'{n / (1024**2):.0f} MB'
    if n >= 1024:
        return f'{n / 1024:.0f} KB'
    return f'{n} B'


def run_benchmark(
    response_size: int,
    n_warmup: int,
    n_rounds: int,
    pool_enabled: bool = True,
) -> dict:
    """Run asymmetric benchmark: int request → bytes response."""
    uid = uuid.uuid4().hex[:8]
    address = f'ipc-v2://asym_{uid}'
    ipc_config = IPCConfig(
        pool_enabled=pool_enabled,
        heartbeat_interval=0,
    )

    server = _start_server(address, ipc_config)
    latencies: list[float] = []

    try:
        with cc.compo.runtime.connect_crm(address, IGenerator, ipc_config=ipc_config) as crm:
            for _ in range(n_warmup):
                crm.generate(response_size)
            for _ in range(n_rounds):
                t0 = time.perf_counter()
                result = crm.generate(response_size)
                latencies.append(time.perf_counter() - t0)
            assert len(result) == response_size
    finally:
        _shutdown(address, server)

    total = sum(latencies)
    ops = n_rounds / total
    throughput_mbs = (response_size * ops) / (1024 * 1024)

    return {
        'pool': pool_enabled,
        'response_size': response_size,
        'size_human': _fmt_size(response_size),
        'rounds': n_rounds,
        'ops_sec': round(ops, 2),
        'throughput_mbs': round(throughput_mbs, 1),
        'avg_ms': round(statistics.mean(latencies) * 1000, 3),
        'p50_ms': round(statistics.median(latencies) * 1000, 3),
        'p95_ms': round(sorted(latencies)[int(n_rounds * 0.95)] * 1000, 3),
        'min_ms': round(min(latencies) * 1000, 3),
        'max_ms': round(max(latencies) * 1000, 3),
        'stdev_ms': round(statistics.stdev(latencies) * 1000, 3) if n_rounds > 1 else 0,
    }


def _rounds(size: int) -> tuple[int, int]:
    if size >= 1024 * 1024 * 1024:
        return 3, 30
    if size >= 512 * 1024 * 1024:
        return 3, 50
    if size >= 256 * 1024 * 1024:
        return 5, 50
    if size >= 100 * 1024 * 1024:
        return 5, 100
    if size >= 10 * 1024 * 1024:
        return 10, 100
    return 20, 100


RESPONSE_SIZES = [
    1 * 1024 * 1024,
    10 * 1024 * 1024,
    50 * 1024 * 1024,
    100 * 1024 * 1024,
    256 * 1024 * 1024,
    512 * 1024 * 1024,
    1024 * 1024 * 1024,
]


def main() -> None:
    print('IPC v2 Asymmetric Benchmark — small request → large response')
    print('Split pool: server response pool auto-expands independently')
    print('=' * 110)
    header = (
        f'{"Response":>10s}  {"Mode":>12s}  '
        f'{"ops/s":>8s}  {"MB/s":>8s}  '
        f'{"avg":>8s}  {"p50":>8s}  {"p95":>8s}  '
        f'{"min":>8s}  {"max":>8s}  {"stdev":>8s}  {"Δ avg":>8s}'
    )
    print(header)
    print('-' * len(header))

    all_results = []

    for size in RESPONSE_SIZES:
        n_warmup, n_rounds = _rounds(size)
        results = {}

        for pool in (True, False):
            try:
                r = run_benchmark(size, n_warmup, n_rounds, pool_enabled=pool)
                results[pool] = r
                all_results.append(r)
            except Exception as e:
                print(f'{_fmt_size(size):>10s}  {"pool" if pool else "per-request":>12s}  ERROR: {e}')
                all_results.append({
                    'pool': pool, 'response_size': size, 'size_human': _fmt_size(size),
                    'rounds': n_rounds, 'error': str(e),
                })
                continue

        if True in results and False in results:
            pool_r = results[True]
            npool_r = results[False]
            speedup = npool_r['avg_ms'] / pool_r['avg_ms'] if pool_r['avg_ms'] > 0 else 0

            for pool, r in [(True, pool_r), (False, npool_r)]:
                mode = 'pool' if pool else 'per-request'
                delta = ''
                if not pool:
                    delta = f'{speedup:.2f}x ▲' if speedup > 1 else f'{1/speedup:.2f}x ▼'
                print(
                    f'{r["size_human"]:>10s}  {mode:>12s}  '
                    f'{r["ops_sec"]:>8.1f}  {r["throughput_mbs"]:>8.1f}  '
                    f'{r["avg_ms"]:>7.3f}  {r["p50_ms"]:>7.3f}  {r["p95_ms"]:>7.3f}  '
                    f'{r["min_ms"]:>7.3f}  {r["max_ms"]:>7.3f}  {r["stdev_ms"]:>7.3f}  {delta:>8s}'
                )
            print()

    out_path = Path(__file__).parent / 'results_asymmetric.json'
    with open(out_path, 'w') as f:
        json.dump({'tag': 'split-pool-asymmetric', 'results': all_results}, f, indent=2)
    print(f'\nResults saved to {out_path}')


if __name__ == '__main__':
    main()
