"""
IPC v3 Detailed Benchmark — 100 rounds, 64B–1GB, focused on P50/throughput/ops/latency.

Usage:
    uv run python benchmarks/ipc_v3_detailed_bench.py
    uv run python benchmarks/ipc_v3_detailed_bench.py --v3-only
    uv run python benchmarks/ipc_v3_detailed_bench.py --json results.json
"""

import argparse
import gc
import json
import math
import os
import statistics
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / 'src'))

import c_two as cc
from c_two.rpc import Server, ServerConfig
from c_two.rpc.server import _start
from c_two.rpc.ipc.ipc_protocol import IPCConfig


@cc.icrm(namespace='bench.echo', version='0.1.0')
class IEcho:
    def echo(self, data: bytes) -> bytes: ...


class Echo:
    def echo(self, data: bytes) -> bytes:
        return data


SIZES = [
    (64,                    '64B'),
    (256,                   '256B'),
    (1024,                  '1KB'),
    (4 * 1024,              '4KB'),
    (16 * 1024,             '16KB'),
    (64 * 1024,             '64KB'),
    (256 * 1024,            '256KB'),
    (1 * 1024 * 1024,       '1MB'),
    (10 * 1024 * 1024,      '10MB'),
    (50 * 1024 * 1024,      '50MB'),
    (100 * 1024 * 1024,     '100MB'),
    (500 * 1024 * 1024,     '500MB'),
    (1024 * 1024 * 1024,    '1GB'),
]

ROUNDS = 100
WARMUP = 5


def percentile(data: list[float], pct: float) -> float:
    if not data:
        return 0.0
    s = sorted(data)
    k = (len(s) - 1) * (pct / 100.0)
    f = int(k)
    c = f + 1
    if c >= len(s):
        return s[-1]
    return s[f] * (c - k) + s[c] * (k - f)


def start_server(address: str, ipc_config: IPCConfig) -> Server:
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


def shutdown(address: str, server: Server) -> None:
    try:
        cc.rpc.Client.shutdown(address, timeout=2.0)
    except Exception:
        pass
    time.sleep(0.1)
    try:
        server.stop()
    except Exception:
        pass


def run_bench(protocol: str, size: int, label: str) -> dict:
    uid = uuid.uuid4().hex[:8]
    address = f'{protocol}://bench_{uid}'
    seg_size = max(512 * 1024 * 1024, size * 2)

    ipc_config = IPCConfig(
        pool_enabled=True,
        pool_segment_size=seg_size,
        max_pool_segments=8,
        max_pool_memory=2 * 1024 * 1024 * 1024,
    )

    server = start_server(address, ipc_config)
    payload = b'\xAB' * size
    latencies: list[float] = []

    try:
        with cc.compo.runtime.connect_crm(address, IEcho, ipc_config=ipc_config) as crm:
            # Warmup
            for _ in range(WARMUP):
                crm.echo(payload)

            # Disable GC during measurement
            gc.disable()
            try:
                for _ in range(ROUNDS):
                    t0 = time.perf_counter()
                    result = crm.echo(payload)
                    elapsed = time.perf_counter() - t0
                    latencies.append(elapsed)
            finally:
                gc.enable()

            assert len(result) == size
    finally:
        shutdown(address, server)

    total = sum(latencies)
    ops = ROUNDS / total if total > 0 else 0
    throughput = (size * 2 * ops) / (1024 * 1024)  # MB/s (round-trip)

    return {
        'protocol': protocol,
        'size': size,
        'label': label,
        'rounds': ROUNDS,
        'p50_ms': statistics.median(latencies) * 1000,
        'p95_ms': percentile(latencies, 95) * 1000,
        'p99_ms': percentile(latencies, 99) * 1000,
        'min_ms': min(latencies) * 1000,
        'max_ms': max(latencies) * 1000,
        'avg_ms': statistics.mean(latencies) * 1000,
        'stddev_ms': statistics.stdev(latencies) * 1000 if len(latencies) > 1 else 0,
        'ops_sec': ops,
        'throughput_mbs': throughput,
    }


def print_single_table(results: list[dict], proto: str) -> None:
    rows = [r for r in results if r['protocol'] == proto]
    if not rows:
        return

    print(f'\n{"=" * 110}')
    print(f'  {proto.upper()} — {ROUNDS} rounds per size, {WARMUP} warmup, GC disabled during measurement')
    print(f'{"=" * 110}')
    hdr = (
        f'{"Size":>7}  │ {"P50(ms)":>10}  {"P95(ms)":>10}  {"Min(ms)":>10}  {"Max(ms)":>10}  '
        f'{"Avg(ms)":>10}  {"StdDev":>8}  │ {"Ops/s":>10}  {"MB/s":>10}'
    )
    print(hdr)
    print('─' * len(hdr))
    for r in rows:
        print(
            f'{r["label"]:>7}  │ '
            f'{r["p50_ms"]:>10.3f}  {r["p95_ms"]:>10.3f}  '
            f'{r["min_ms"]:>10.3f}  {r["max_ms"]:>10.3f}  '
            f'{r["avg_ms"]:>10.3f}  {r["stddev_ms"]:>8.3f}  │ '
            f'{r["ops_sec"]:>10.1f}  {r["throughput_mbs"]:>10.1f}'
        )


def print_comparison(results: list[dict]) -> None:
    sizes = sorted(set(r['size'] for r in results))
    by_key = {(r['protocol'], r['size']): r for r in results}

    print(f'\n{"=" * 120}')
    print(f'  V2 vs V3 COMPARISON — Δ% = (v3 - v2) / v2 × 100  (negative = v3 faster)')
    print(f'{"=" * 120}')
    hdr = (
        f'{"Size":>7}  │ {"v2 P50":>10}  {"v3 P50":>10}  {"Δ P50%":>8}  │ '
        f'{"v2 Ops/s":>10}  {"v3 Ops/s":>10}  {"Δ Ops%":>8}  │ '
        f'{"v2 MB/s":>10}  {"v3 MB/s":>10}  {"Δ TP%":>8}'
    )
    print(hdr)
    print('─' * len(hdr))

    for sz in sizes:
        v2 = by_key.get(('ipc-v2', sz))
        v3 = by_key.get(('ipc-v3', sz))
        if not v2 or not v3:
            continue

        dp50 = ((v3['p50_ms'] - v2['p50_ms']) / v2['p50_ms'] * 100) if v2['p50_ms'] > 0 else 0
        dops = ((v3['ops_sec'] - v2['ops_sec']) / v2['ops_sec'] * 100) if v2['ops_sec'] > 0 else 0
        dtp = ((v3['throughput_mbs'] - v2['throughput_mbs']) / v2['throughput_mbs'] * 100) if v2['throughput_mbs'] > 0 else 0

        p50_sym = '✓' if dp50 < -1 else ('✗' if dp50 > 1 else '≈')
        ops_sym = '✓' if dops > 1 else ('✗' if dops < -1 else '≈')
        tp_sym = '✓' if dtp > 1 else ('✗' if dtp < -1 else '≈')

        lbl = [r for r in results if r['size'] == sz][0]['label']
        print(
            f'{lbl:>7}  │ '
            f'{v2["p50_ms"]:>10.3f}  {v3["p50_ms"]:>10.3f}  {dp50:>+7.1f}{p50_sym} │ '
            f'{v2["ops_sec"]:>10.1f}  {v3["ops_sec"]:>10.1f}  {dops:>+7.1f}{ops_sym} │ '
            f'{v2["throughput_mbs"]:>10.1f}  {v3["throughput_mbs"]:>10.1f}  {dtp:>+7.1f}{tp_sym}'
        )

    # Summary
    v3_wins = sum(1 for sz in sizes if by_key.get(('ipc-v3', sz), {}).get('p50_ms', float('inf')) < by_key.get(('ipc-v2', sz), {}).get('p50_ms', 0))
    print(f'\n  Summary: v3 faster in {v3_wins}/{len(sizes)} sizes (P50 latency)')


def main():
    parser = argparse.ArgumentParser(description='IPC v3 Detailed Benchmark')
    parser.add_argument('--v2-only', action='store_true')
    parser.add_argument('--v3-only', action='store_true')
    parser.add_argument('--json', type=str, help='Save JSON results')
    args = parser.parse_args()

    protocols = []
    if not args.v3_only:
        protocols.append('ipc-v2')
    if not args.v2_only:
        protocols.append('ipc-v3')

    print(f'IPC Detailed Benchmark — {ROUNDS} rounds, GC disabled, {len(SIZES)} sizes')
    print(f'Protocols: {", ".join(protocols)}')
    print(f'Sizes: {", ".join(s[1] for s in SIZES)}')
    print()

    results: list[dict] = []

    for proto in protocols:
        print(f'--- {proto.upper()} ---')
        for size, label in SIZES:
            print(f'  {label:>7}: {ROUNDS} rounds ... ', end='', flush=True)
            try:
                r = run_bench(proto, size, label)
                results.append(r)
                print(
                    f'P50={r["p50_ms"]:.3f}ms  '
                    f'Ops={r["ops_sec"]:.1f}/s  '
                    f'TP={r["throughput_mbs"]:.1f}MB/s  '
                    f'Lat={r["avg_ms"]:.3f}±{r["stddev_ms"]:.3f}ms'
                )
            except Exception as e:
                print(f'FAILED: {e}')
                import traceback
                traceback.print_exc()
        print()

    for proto in protocols:
        print_single_table(results, proto)

    if len(protocols) == 2:
        print_comparison(results)

    if args.json:
        with open(args.json, 'w') as f:
            json.dump(results, f, indent=2, default=str)
        print(f'\nResults saved to {args.json}')


if __name__ == '__main__':
    main()
