"""
IPC v2 vs v3 Benchmark — compare half-duplex pool SHM (v2) against
buddy allocator SHM (v3) across payload sizes from 64 B to 1 GB.

Metrics: P50/P95/P99 latency, throughput (MB/s), ops/sec.
Also includes a random-size stress test for allocator stability.

Usage:
    uv run python benchmarks/ipc_v2_vs_v3_benchmark.py
    uv run python benchmarks/ipc_v2_vs_v3_benchmark.py --quick   # fewer rounds
    uv run python benchmarks/ipc_v2_vs_v3_benchmark.py --stress  # random-size test
"""

import argparse
import json
import math
import os
import random
import statistics
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / 'src'))

import c_two as cc                                           # noqa: E402
from c_two.rpc import Server, ServerConfig                   # noqa: E402
from c_two.rpc.server import _start                          # noqa: E402
from c_two.rpc.ipc.ipc_protocol import IPCConfig             # noqa: E402


# ---------------------------------------------------------------------------
# Echo CRM for benchmarking
# ---------------------------------------------------------------------------

@cc.icrm(namespace='bench.echo', version='0.1.0')
class IEcho:
    def echo(self, data: bytes) -> bytes: ...


class Echo:
    def echo(self, data: bytes) -> bytes:
        return data


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _start_server(address: str, ipc_config: IPCConfig | None = None) -> Server:
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
    if n >= 1_073_741_824:
        return f'{n / 1_073_741_824:.1f}GB'
    if n >= 1_048_576:
        return f'{n / 1_048_576:.0f}MB'
    if n >= 1024:
        return f'{n / 1024:.0f}KB'
    return f'{n}B'


def _percentile(data: list[float], pct: float) -> float:
    idx = int(len(data) * pct / 100)
    idx = min(idx, len(data) - 1)
    return sorted(data)[idx]


# ---------------------------------------------------------------------------
# Core benchmark function
# ---------------------------------------------------------------------------

def run_echo(
    protocol: str,
    payload_size: int,
    n_warmup: int,
    n_rounds: int,
    pool_segment_size: int = 512 * 1024 * 1024,
) -> dict:
    uid = uuid.uuid4().hex[:8]
    address = f'{protocol}://bench_{uid}'

    ipc_config = IPCConfig(
        pool_enabled=True,
        pool_segment_size=pool_segment_size,
        max_pool_segments=8,
        max_pool_memory=2 * 1024 * 1024 * 1024,
    )

    server = _start_server(address, ipc_config)
    payload = b'\xAB' * payload_size
    latencies: list[float] = []

    try:
        with cc.compo.runtime.connect_crm(address, IEcho, ipc_config=ipc_config) as crm:
            for _ in range(n_warmup):
                crm.echo(payload)

            for _ in range(n_rounds):
                t0 = time.perf_counter()
                result = crm.echo(payload)
                elapsed = time.perf_counter() - t0
                latencies.append(elapsed)

            assert len(result) == payload_size, f'Expected {payload_size}, got {len(result)}'
    finally:
        _shutdown(address, server)

    total = sum(latencies)
    ops = n_rounds / total if total > 0 else 0
    throughput_mbs = (payload_size * 2 * ops) / (1024 * 1024)

    return {
        'protocol': protocol,
        'size': payload_size,
        'size_label': _fmt_size(payload_size),
        'rounds': n_rounds,
        'ops_sec': round(ops, 2),
        'throughput_mbs': round(throughput_mbs, 2),
        'p50_ms': round(statistics.median(latencies) * 1000, 3),
        'p95_ms': round(_percentile(latencies, 95) * 1000, 3),
        'p99_ms': round(_percentile(latencies, 99) * 1000, 3),
        'min_ms': round(min(latencies) * 1000, 3),
        'max_ms': round(max(latencies) * 1000, 3),
        'avg_ms': round(statistics.mean(latencies) * 1000, 3),
    }


# ---------------------------------------------------------------------------
# Random-size stress test
# ---------------------------------------------------------------------------

def run_stress(
    protocol: str,
    n_calls: int = 200,
    pool_segment_size: int = 512 * 1024 * 1024,
) -> dict:
    """Random payload sizes from 64B to 10MB, testing allocator stability."""
    uid = uuid.uuid4().hex[:8]
    address = f'{protocol}://stress_{uid}'

    ipc_config = IPCConfig(
        pool_enabled=True,
        pool_segment_size=pool_segment_size,
        max_pool_segments=8,
        max_pool_memory=2 * 1024 * 1024 * 1024,
    )

    # Random sizes across multiple orders of magnitude.
    sizes = []
    for _ in range(n_calls):
        exp = random.uniform(math.log2(64), math.log2(10 * 1024 * 1024))
        sizes.append(int(2 ** exp))

    server = _start_server(address, ipc_config)
    latencies: list[float] = []
    errors = 0

    try:
        with cc.compo.runtime.connect_crm(address, IEcho, ipc_config=ipc_config) as crm:
            # Warmup
            crm.echo(b'\x00' * 1024)

            for sz in sizes:
                payload = b'\xAB' * sz
                try:
                    t0 = time.perf_counter()
                    result = crm.echo(payload)
                    elapsed = time.perf_counter() - t0
                    latencies.append(elapsed)
                    assert len(result) == sz
                except Exception:
                    errors += 1
    finally:
        _shutdown(address, server)

    total = sum(latencies)
    ops = len(latencies) / total if total > 0 else 0
    total_bytes = sum(sizes[:len(latencies)]) * 2

    return {
        'protocol': protocol,
        'calls': n_calls,
        'successful': len(latencies),
        'errors': errors,
        'ops_sec': round(ops, 2),
        'throughput_mbs': round(total_bytes / total / 1_048_576, 2) if total > 0 else 0,
        'p50_ms': round(statistics.median(latencies) * 1000, 3) if latencies else 0,
        'p95_ms': round(_percentile(latencies, 95) * 1000, 3) if latencies else 0,
        'min_ms': round(min(latencies) * 1000, 3) if latencies else 0,
        'max_ms': round(max(latencies) * 1000, 3) if latencies else 0,
    }


# ---------------------------------------------------------------------------
# Display
# ---------------------------------------------------------------------------

def print_table(results: list[dict]) -> None:
    print()
    header = f'{"Size":>8}  {"Proto":>6}  {"P50(ms)":>10}  {"P95(ms)":>10}  {"P99(ms)":>10}  {"Avg(ms)":>10}  {"Ops/s":>10}  {"MB/s":>10}'
    print(header)
    print('-' * len(header))
    for r in results:
        print(
            f'{r["size_label"]:>8}  {r["protocol"]:>6}  '
            f'{r["p50_ms"]:>10.3f}  {r["p95_ms"]:>10.3f}  {r["p99_ms"]:>10.3f}  '
            f'{r["avg_ms"]:>10.3f}  {r["ops_sec"]:>10.1f}  {r["throughput_mbs"]:>10.1f}'
        )


def print_comparison(results: list[dict]) -> None:
    """Print a side-by-side v2 vs v3 comparison."""
    sizes = sorted(set(r['size'] for r in results))
    by_key = {(r['protocol'], r['size']): r for r in results}

    print()
    header = f'{"Size":>8}  {"v2 P50":>10}  {"v3 P50":>10}  {"Δ%":>8}  {"v2 MB/s":>10}  {"v3 MB/s":>10}  {"Δ%":>8}'
    print(header)
    print('-' * len(header))

    for sz in sizes:
        v2 = by_key.get(('ipc-v2', sz))
        v3 = by_key.get(('ipc-v3', sz))
        if not v2 or not v3:
            continue

        p50_pct = ((v3['p50_ms'] - v2['p50_ms']) / v2['p50_ms'] * 100) if v2['p50_ms'] > 0 else 0
        tp_pct = ((v3['throughput_mbs'] - v2['throughput_mbs']) / v2['throughput_mbs'] * 100) if v2['throughput_mbs'] > 0 else 0

        p50_color = '↓' if p50_pct < 0 else '↑'
        tp_color = '↑' if tp_pct > 0 else '↓'

        print(
            f'{_fmt_size(sz):>8}  {v2["p50_ms"]:>10.3f}  {v3["p50_ms"]:>10.3f}  '
            f'{p50_pct:>+7.1f}{p50_color}  '
            f'{v2["throughput_mbs"]:>10.1f}  {v3["throughput_mbs"]:>10.1f}  '
            f'{tp_pct:>+7.1f}{tp_color}'
        )


# ---------------------------------------------------------------------------
# Payload/round configs
# ---------------------------------------------------------------------------

FULL_SIZES = [
    64,                        # 64 B
    256,                       # 256 B
    1 * 1024,                  # 1 KB
    4 * 1024,                  # 4 KB (SHM threshold)
    16 * 1024,                 # 16 KB
    64 * 1024,                 # 64 KB
    256 * 1024,                # 256 KB
    1 * 1024 * 1024,           # 1 MB
    10 * 1024 * 1024,          # 10 MB
    100 * 1024 * 1024,         # 100 MB
    512 * 1024 * 1024,         # 512 MB
    1024 * 1024 * 1024,        # 1 GB
]

QUICK_SIZES = [
    64,
    1 * 1024,
    64 * 1024,
    1 * 1024 * 1024,
    10 * 1024 * 1024,
    100 * 1024 * 1024,
]


def _rounds(size: int, quick: bool = False) -> tuple[int, int]:
    """Return (warmup, rounds) based on payload size."""
    if quick:
        if size >= 100 * 1024 * 1024:
            return 1, 3
        if size >= 10 * 1024 * 1024:
            return 2, 5
        if size >= 1 * 1024 * 1024:
            return 3, 10
        return 5, 30
    else:
        if size >= 512 * 1024 * 1024:
            return 2, 5
        if size >= 100 * 1024 * 1024:
            return 3, 10
        if size >= 10 * 1024 * 1024:
            return 5, 20
        if size >= 1 * 1024 * 1024:
            return 10, 50
        return 20, 200


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description='IPC v2 vs v3 benchmark')
    parser.add_argument('--quick', action='store_true', help='Fewer sizes and rounds')
    parser.add_argument('--stress', action='store_true', help='Run random-size stress test')
    parser.add_argument('--v2-only', action='store_true', help='Only benchmark IPC v2')
    parser.add_argument('--v3-only', action='store_true', help='Only benchmark IPC v3')
    parser.add_argument('--json', type=str, help='Save results to JSON file')
    args = parser.parse_args()

    sizes = QUICK_SIZES if args.quick else FULL_SIZES
    protocols = []
    if not args.v3_only:
        protocols.append('ipc-v2')
    if not args.v2_only:
        protocols.append('ipc-v3')

    print('=' * 70)
    print('IPC v2 vs v3 Benchmark')
    print(f'Protocols: {", ".join(protocols)}')
    print(f'Sizes: {", ".join(_fmt_size(s) for s in sizes)}')
    print(f'Mode: {"quick" if args.quick else "full"}')
    print('=' * 70)

    results: list[dict] = []

    for proto in protocols:
        print(f'\n--- {proto.upper()} ---')
        for size in sizes:
            warmup, rounds = _rounds(size, args.quick)
            seg_size = max(512 * 1024 * 1024, size * 2)
            print(f'  {_fmt_size(size):>8}: {rounds} rounds, {warmup} warmup ... ', end='', flush=True)

            try:
                r = run_echo(proto, size, warmup, rounds, pool_segment_size=seg_size)
                results.append(r)
                print(f'P50={r["p50_ms"]:.3f}ms  Ops={r["ops_sec"]:.1f}/s  TP={r["throughput_mbs"]:.1f}MB/s')
            except Exception as e:
                print(f'FAILED: {e}')

    print_table(results)

    if len(protocols) == 2:
        print('\n--- COMPARISON (v3 vs v2) ---')
        print_comparison(results)

    # Stress test
    if args.stress:
        print('\n--- RANDOM-SIZE STRESS TEST ---')
        n_stress = 50 if args.quick else 200
        for proto in protocols:
            print(f'  {proto}: {n_stress} random calls ... ', end='', flush=True)
            try:
                sr = run_stress(proto, n_calls=n_stress)
                print(
                    f'OK ({sr["successful"]}/{sr["calls"]}, '
                    f'P50={sr["p50_ms"]:.3f}ms, '
                    f'{sr["throughput_mbs"]:.1f}MB/s, '
                    f'{sr["errors"]} errors)'
                )
            except Exception as e:
                print(f'FAILED: {e}')

    # Save results
    if args.json:
        with open(args.json, 'w') as f:
            json.dump(results, f, indent=2)
        print(f'\nResults saved to {args.json}')


if __name__ == '__main__':
    main()
