"""Benchmark: hold vs view buffer mode — payload 64B to 1GB.

Measures P50 round-trip latency comparing:
  - view mode (default): SHM released immediately after deserialize
  - hold mode (cc.hold()): response wrapped in HeldResult, released by caller

Uses thread-local transport (same process) to isolate buffer-mode overhead
from IPC transport noise.

Payload sizes: 64B, 256B, 1KB, 4KB, 64KB, 1MB, 10MB, 50MB, 100MB, 500MB, 1GB

Results are written to benchmarks/results/ (git-ignored).

Usage:
    C2_RELAY_ADDRESS= uv run python benchmarks/hold_vs_view_benchmark.py
    C2_RELAY_ADDRESS= uv run python benchmarks/hold_vs_view_benchmark.py --max-mb 100
"""
from __future__ import annotations

import argparse
import gc
import os
import statistics
import sys
import time

os.environ.setdefault('C2_RELAY_ADDRESS', '')
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))

import c_two as cc
from c_two.transport.registry import _ProcessRegistry

# ---------------------------------------------------------------------------
# Echo CRM — identity fast path (bytes in, bytes out)
# ---------------------------------------------------------------------------

@cc.icrm(namespace='bench.hold_view', version='0.1.0')
class IEcho:
    def echo(self, data: bytes) -> bytes: ...

class Echo:
    def echo(self, data: bytes) -> bytes:
        return data


# ---------------------------------------------------------------------------
# Size matrix
# ---------------------------------------------------------------------------

SIZES = [
    (64,                    '64B'),
    (256,                   '256B'),
    (1024,                  '1KB'),
    (4 * 1024,              '4KB'),
    (64 * 1024,             '64KB'),
    (1024 * 1024,           '1MB'),
    (10 * 1024 * 1024,      '10MB'),
    (50 * 1024 * 1024,      '50MB'),
    (100 * 1024 * 1024,     '100MB'),
    (500 * 1024 * 1024,     '500MB'),
    (1024 * 1024 * 1024,    '1GB'),
]

WARMUP = 3


def _rounds(size: int) -> int:
    if size <= 1024 * 1024:
        return 100
    if size <= 100 * 1024 * 1024:
        return 20
    return 5


def _fmt(v: float) -> str:
    if v < 1.0:
        return f'{v:.4f}'
    if v < 100:
        return f'{v:.3f}'
    return f'{v:.1f}'


# ---------------------------------------------------------------------------
# Measurement helpers
# ---------------------------------------------------------------------------

def _measure_view(proxy, payload: bytes, rounds: int) -> float:
    """Warmup + timed rounds for view mode (default). Returns P50 ms."""
    for _ in range(WARMUP):
        proxy.echo(payload)

    latencies: list[float] = []
    gc.disable()
    try:
        for _ in range(rounds):
            t0 = time.perf_counter()
            result = proxy.echo(payload)
            elapsed = time.perf_counter() - t0
            latencies.append(elapsed)
    finally:
        gc.enable()

    assert len(result) == len(payload), f'size mismatch: {len(result)} != {len(payload)}'
    return statistics.median(latencies) * 1000


def _measure_hold(proxy, payload: bytes, rounds: int) -> float:
    """Warmup + timed rounds for hold mode. Returns P50 ms."""
    for _ in range(WARMUP):
        held = cc.hold(proxy.echo)(payload)
        held.release()

    latencies: list[float] = []
    gc.disable()
    try:
        for _ in range(rounds):
            t0 = time.perf_counter()
            held = cc.hold(proxy.echo)(payload)
            elapsed = time.perf_counter() - t0
            latencies.append(elapsed)
            held.release()
    finally:
        gc.enable()

    return statistics.median(latencies) * 1000


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description='Hold vs View buffer mode benchmark')
    parser.add_argument('--max-mb', type=int, default=None,
                        help='Cap max payload size in MB (skip sizes above)')
    args = parser.parse_args()

    max_bytes = args.max_mb * 1024 * 1024 if args.max_mb is not None else None
    active_sizes = [(s, l) for s, l in SIZES if max_bytes is None or s <= max_bytes]

    # Setup
    _ProcessRegistry.reset()
    cc.register(IEcho, Echo(), name='echo_hv')
    proxy = cc.connect(IEcho, name='echo_hv')

    print()
    print('Hold vs View Buffer Mode Benchmark')
    print('=' * 60)
    print(f'Transport: thread-local  |  Warmup: {WARMUP}  |  Adaptive rounds')
    print(f'Python: {sys.version}')
    print('=' * 60)
    print(f'{"Size":>8s}  {"Rounds":>6s}  {"View P50(ms)":>13s}  '
          f'{"Hold P50(ms)":>13s}  {"Ratio(H/V)":>11s}')
    print('-' * 60)

    results: list[dict] = []

    for size_bytes, label in active_sizes:
        rounds = _rounds(size_bytes)
        payload = b'\xAB' * size_bytes

        view_ms = _measure_view(proxy, payload, rounds)
        hold_ms = _measure_hold(proxy, payload, rounds)

        ratio = hold_ms / view_ms if view_ms > 0 else 0.0

        print(f'{label:>8s}  {rounds:>6d}  {_fmt(view_ms):>13s}  '
              f'{_fmt(hold_ms):>13s}  {ratio:>10.2f}x')

        results.append({
            'size': label, 'size_bytes': size_bytes, 'rounds': rounds,
            'view_ms': view_ms, 'hold_ms': hold_ms, 'ratio': ratio,
        })

        del payload
        gc.collect()

    print('=' * 60)

    # Cleanup proxy and CRM
    cc.close(proxy)
    cc.unregister('echo_hv')
    cc.shutdown()
    _ProcessRegistry.reset()

    # Write results
    results_dir = os.path.join(os.path.dirname(__file__), 'results')
    os.makedirs(results_dir, exist_ok=True)
    out_path = os.path.join(results_dir, 'hold_vs_view.txt')
    with open(out_path, 'w') as f:
        f.write(f'{"Size":>8s}\t{"Rounds":>6s}\t{"View P50(ms)":>13s}\t'
                f'{"Hold P50(ms)":>13s}\t{"Ratio(H/V)":>11s}\n')
        for r in results:
            f.write(f'{r["size"]:>8s}\t{r["rounds"]:>6d}\t{r["view_ms"]:>13.4f}\t'
                    f'{r["hold_ms"]:>13.4f}\t{r["ratio"]:>10.2f}x\n')
    print(f'\nResults written to {os.path.abspath(out_path)}')


if __name__ == '__main__':
    main()
