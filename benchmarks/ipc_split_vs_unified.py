"""
IPC v2 Split Pool vs Unified Pool — 100-round regression benchmark.

Runs two workload types at each payload size:
  1. Echo (symmetric): request = response size (both pools exercise equally)
  2. Generator (asymmetric): small int request → large bytes response

For each workload × size × mode (pool / per-request), runs 100 rounds
(fewer for ≥512 MB) and reports latency stats + throughput.

Usage:
    uv run python benchmarks/ipc_split_vs_unified.py [--tag <label>]
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
# CRMs
# ---------------------------------------------------------------------------

@cc.icrm(namespace='bench.echo', version='0.1.0')
class IEcho:
    def echo(self, data: bytes) -> bytes: ...

class Echo:
    def echo(self, data: bytes) -> bytes:
        return data


@cc.icrm(namespace='bench.gen', version='0.1.0')
class IGenerator:
    def generate(self, size: int) -> bytes: ...

class Generator:
    def generate(self, size: int) -> bytes:
        return b'\xBB' * size


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _start_server(address, crm, icrm, ipc_config):
    server = Server(ServerConfig(
        name='Bench', crm=crm, icrm=icrm,
        bind_address=address, ipc_config=ipc_config,
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


def _fmt(n):
    if n >= 1 << 30: return f'{n / (1 << 30):.0f} GB'
    if n >= 1 << 20: return f'{n / (1 << 20):.0f} MB'
    if n >= 1 << 10: return f'{n / (1 << 10):.0f} KB'
    return f'{n} B'


def _rounds(size):
    if size >= 1 << 30: return 3, 30
    if size >= 512 * (1 << 20): return 3, 50
    if size >= 256 * (1 << 20): return 5, 50
    if size >= 100 * (1 << 20): return 5, 100
    if size >= 10 * (1 << 20): return 10, 100
    return 20, 100


# ---------------------------------------------------------------------------
# Benchmark runners
# ---------------------------------------------------------------------------

def bench_echo(payload_size, n_warmup, n_rounds, pool_enabled=True):
    uid = uuid.uuid4().hex[:8]
    addr = f'ipc-v2://be_{uid}'
    cfg = IPCConfig(pool_enabled=pool_enabled, heartbeat_interval=0)
    server = _start_server(addr, Echo(), IEcho, cfg)
    payload = b'\xAB' * payload_size
    latencies = []
    try:
        with cc.compo.runtime.connect_crm(addr, IEcho, ipc_config=cfg) as crm:
            for _ in range(n_warmup):
                crm.echo(payload)
            for _ in range(n_rounds):
                t0 = time.perf_counter()
                result = crm.echo(payload)
                latencies.append(time.perf_counter() - t0)
            assert len(result) == payload_size
    finally:
        _shutdown(addr, server)
    return _stats(latencies, n_rounds, payload_size * 2, pool_enabled, payload_size, 'echo')


def bench_generator(response_size, n_warmup, n_rounds, pool_enabled=True):
    uid = uuid.uuid4().hex[:8]
    addr = f'ipc-v2://bg_{uid}'
    cfg = IPCConfig(pool_enabled=pool_enabled, heartbeat_interval=0)
    server = _start_server(addr, Generator(), IGenerator, cfg)
    latencies = []
    try:
        with cc.compo.runtime.connect_crm(addr, IGenerator, ipc_config=cfg) as crm:
            for _ in range(n_warmup):
                crm.generate(response_size)
            for _ in range(n_rounds):
                t0 = time.perf_counter()
                result = crm.generate(response_size)
                latencies.append(time.perf_counter() - t0)
            assert len(result) == response_size
    finally:
        _shutdown(addr, server)
    return _stats(latencies, n_rounds, response_size, pool_enabled, response_size, 'generator')


def _stats(latencies, n_rounds, data_bytes_per_call, pool_enabled, size, workload):
    total = sum(latencies)
    ops = n_rounds / total
    throughput = (data_bytes_per_call * ops) / (1 << 20)
    return {
        'workload': workload,
        'pool': pool_enabled,
        'size': size,
        'size_human': _fmt(size),
        'rounds': n_rounds,
        'ops_sec': round(ops, 2),
        'throughput_mbs': round(throughput, 1),
        'avg_ms': round(statistics.mean(latencies) * 1000, 3),
        'p50_ms': round(statistics.median(latencies) * 1000, 3),
        'p95_ms': round(sorted(latencies)[int(n_rounds * 0.95)] * 1000, 3),
        'p99_ms': round(sorted(latencies)[min(int(n_rounds * 0.99), n_rounds - 1)] * 1000, 3),
        'min_ms': round(min(latencies) * 1000, 3),
        'max_ms': round(max(latencies) * 1000, 3),
        'stdev_ms': round(statistics.stdev(latencies) * 1000, 3) if n_rounds > 1 else 0,
        'latencies': [round(l * 1000, 4) for l in latencies],
    }


# ---------------------------------------------------------------------------
# Payload sizes
# ---------------------------------------------------------------------------

SIZES = [
    512,
    4 * 1024,
    16 * 1024,
    64 * 1024,
    256 * 1024,
    1 * (1 << 20),
    4 * (1 << 20),
    10 * (1 << 20),
    50 * (1 << 20),
    100 * (1 << 20),
    256 * (1 << 20),
    512 * (1 << 20),
    1 * (1 << 30),
]


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def _print_header():
    h = (
        f'{"Workload":>10s}  {"Payload":>8s}  {"Mode":>12s}  '
        f'{"ops/s":>8s}  {"MB/s":>8s}  '
        f'{"avg":>8s}  {"p50":>8s}  {"p95":>8s}  {"p99":>8s}  '
        f'{"min":>8s}  {"max":>8s}  {"stdev":>8s}  {"Δ avg":>8s}'
    )
    print(h)
    print('-' * len(h))
    return h


def _print_row(r, delta=''):
    mode = 'pool' if r['pool'] else 'per-req'
    print(
        f'{r["workload"]:>10s}  {r["size_human"]:>8s}  {mode:>12s}  '
        f'{r["ops_sec"]:>8.1f}  {r["throughput_mbs"]:>8.1f}  '
        f'{r["avg_ms"]:>7.3f}  {r["p50_ms"]:>7.3f}  {r["p95_ms"]:>7.3f}  {r["p99_ms"]:>7.3f}  '
        f'{r["min_ms"]:>7.3f}  {r["max_ms"]:>7.3f}  {r["stdev_ms"]:>7.3f}  {delta:>8s}'
    )


def main():
    tag = 'benchmark'
    for i, a in enumerate(sys.argv[1:]):
        if a == '--tag' and i + 2 < len(sys.argv):
            tag = sys.argv[i + 2]
            break
    if len(sys.argv) > 1 and sys.argv[1] != '--tag':
        tag = sys.argv[1]

    print(f'IPC v2 Benchmark — tag: {tag}')
    print(f'Echo (symmetric) + Generator (asymmetric), 100 rounds per size')
    print('=' * 140)

    _print_header()

    all_results = []

    for workload_name, bench_fn in [('echo', bench_echo), ('generator', bench_generator)]:
        for size in SIZES:
            n_warmup, n_rounds = _rounds(size)
            results = {}

            for pool in (True, False):
                try:
                    r = bench_fn(size, n_warmup, n_rounds, pool_enabled=pool)
                    results[pool] = r
                    # Store without latencies array for summary
                    summary = {k: v for k, v in r.items() if k != 'latencies'}
                    all_results.append(summary)
                except Exception as e:
                    print(f'{workload_name:>10s}  {_fmt(size):>8s}  {"pool" if pool else "per-req":>12s}  ERROR: {e}')
                    all_results.append({
                        'workload': workload_name, 'pool': pool,
                        'size': size, 'size_human': _fmt(size),
                        'rounds': n_rounds, 'error': str(e),
                    })

            if True in results and False in results:
                pr, nr = results[True], results[False]
                speedup = nr['avg_ms'] / pr['avg_ms'] if pr['avg_ms'] > 0 else 0
                _print_row(pr)
                delta = f'{speedup:.2f}x ▲' if speedup > 1 else f'{1/speedup:.2f}x ▼'
                _print_row(nr, delta)
                print()
            elif True in results:
                _print_row(results[True])
                print()

        print()  # blank line between workloads

    # Save full results (with latencies) as JSON
    out = Path(__file__).parent / f'results_{tag}.json'
    with open(out, 'w') as f:
        json.dump({'tag': tag, 'results': all_results}, f, indent=2)
    print(f'\nResults saved to {out}')


if __name__ == '__main__':
    main()
