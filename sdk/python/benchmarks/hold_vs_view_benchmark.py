"""Benchmark: hold (zero-copy) vs view (copy) for numpy grid data.

Demonstrates the core value of hold mode: avoiding numpy array copies.
  - view mode: np.frombuffer(buf).copy() — must copy because SHM is released
  - hold mode: np.frombuffer(buf)         — zero-copy, SHM stays alive

Uses structured numpy arrays matching GridAttribute schema (9 columns,
~42 bytes/row) to simulate realistic grid workloads. Each measurement
includes a columnar operation (elevation.sum()) to verify data is usable.

Payload sizes: 64B .. 1GB

Usage:
    C2_RELAY_ADDRESS= uv run python sdk/python/benchmarks/hold_vs_view_benchmark.py
    C2_RELAY_ADDRESS= uv run python sdk/python/benchmarks/hold_vs_view_benchmark.py --max-mb 100
"""
from __future__ import annotations

import argparse
import gc
import os
import statistics
import sys
import time

import numpy as np

# ---------------------------------------------------------------------------
# Grid dtype — mirrors GridAttribute from examples/python/grid
# ---------------------------------------------------------------------------

GRID_DTYPE = np.dtype([
    ('level',      np.int8),
    ('type',       np.int8),
    ('activate',   np.uint8),
    ('global_id',  np.int32),
    ('elevation',  np.float64),
    ('min_x',      np.float64),
    ('min_y',      np.float64),
    ('max_x',      np.float64),
    ('max_y',      np.float64),
])

ROW_SIZE = GRID_DTYPE.itemsize  # bytes per row

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
        return 200
    if size <= 100 * 1024 * 1024:
        return 30
    return 5


def _fmt(v: float) -> str:
    if v < 0.001:
        return f'{v * 1000:.2f}µs'
    if v < 1.0:
        return f'{v:.4f}'
    if v < 100:
        return f'{v:.2f}'
    return f'{v:.1f}'


def _generate_grid(n_rows: int) -> bytes:
    """Generate random grid data as raw bytes."""
    rng = np.random.default_rng(42)
    grid = np.empty(n_rows, dtype=GRID_DTYPE)
    grid['level'] = rng.integers(0, 10, n_rows, dtype=np.int8)
    grid['type'] = rng.integers(0, 5, n_rows, dtype=np.int8)
    grid['activate'] = rng.integers(0, 2, n_rows, dtype=np.uint8)
    grid['global_id'] = rng.integers(0, 1_000_000, n_rows, dtype=np.int32)
    grid['elevation'] = rng.uniform(-1000, 5000, n_rows)
    grid['min_x'] = rng.uniform(-180, 180, n_rows)
    grid['min_y'] = rng.uniform(-90, 90, n_rows)
    grid['max_x'] = grid['min_x'] + rng.uniform(0.001, 1, n_rows)
    grid['max_y'] = grid['min_y'] + rng.uniform(0.001, 1, n_rows)
    return grid.tobytes()


# ---------------------------------------------------------------------------
# Measurement: what view mode costs vs what hold mode costs
# ---------------------------------------------------------------------------

def _measure_copy(buf: bytes, n_rows: int, rounds: int) -> float:
    """View mode path: frombuffer + copy + columnar access. Returns P50 ms."""
    for _ in range(WARMUP):
        arr = np.frombuffer(buf, dtype=GRID_DTYPE, count=n_rows).copy()
        _ = arr['elevation'].sum()

    latencies: list[float] = []
    gc.disable()
    try:
        for _ in range(rounds):
            t0 = time.perf_counter()
            arr = np.frombuffer(buf, dtype=GRID_DTYPE, count=n_rows).copy()
            _ = arr['elevation'].sum()
            elapsed = time.perf_counter() - t0
            latencies.append(elapsed)
    finally:
        gc.enable()

    return statistics.median(latencies) * 1000


def _measure_view(buf: bytes, n_rows: int, rounds: int) -> float:
    """Hold mode path: frombuffer (zero-copy) + columnar access. Returns P50 ms."""
    for _ in range(WARMUP):
        arr = np.frombuffer(buf, dtype=GRID_DTYPE, count=n_rows)
        _ = arr['elevation'].sum()

    latencies: list[float] = []
    gc.disable()
    try:
        for _ in range(rounds):
            t0 = time.perf_counter()
            arr = np.frombuffer(buf, dtype=GRID_DTYPE, count=n_rows)
            _ = arr['elevation'].sum()
            elapsed = time.perf_counter() - t0
            latencies.append(elapsed)
    finally:
        gc.enable()

    return statistics.median(latencies) * 1000


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description='Hold (zero-copy) vs View (copy) benchmark for numpy grid data')
    parser.add_argument('--max-mb', type=int, default=None,
                        help='Cap max payload size in MB')
    args = parser.parse_args()

    max_bytes = args.max_mb * 1024 * 1024 if args.max_mb is not None else None
    active_sizes = [(s, l) for s, l in SIZES if max_bytes is None or s <= max_bytes]

    print()
    print('Hold vs View: Grid Numpy Benchmark')
    print('=' * 78)
    print(f'Grid dtype: {len(GRID_DTYPE)} columns, {ROW_SIZE} bytes/row')
    print(f'Columnar op: elevation.sum()  |  Warmup: {WARMUP}  |  Adaptive rounds')
    print(f'Python: {sys.version}')
    print(f'NumPy:  {np.__version__}')
    print('=' * 78)
    hdr = (f'{"Size":>8s}  {"Rows":>12s}  {"Rounds":>6s}  '
           f'{"Copy P50":>12s}  {"View P50":>12s}  {"Speedup":>8s}')
    print(hdr)
    print(f'{"":>8s}  {"":>12s}  {"":>6s}  '
          f'{"(view mode)":>12s}  {"(hold mode)":>12s}')
    print('-' * 78)

    results: list[dict] = []

    for size_bytes, label in active_sizes:
        n_rows = max(1, size_bytes // ROW_SIZE)
        actual_bytes = n_rows * ROW_SIZE
        rounds = _rounds(actual_bytes)

        sys.stdout.write(f'{label:>8s}  {n_rows:>12,d}  {rounds:>6d}  ')
        sys.stdout.flush()

        buf = _generate_grid(n_rows)

        copy_ms = _measure_copy(buf, n_rows, rounds)
        view_ms = _measure_view(buf, n_rows, rounds)

        speedup = copy_ms / view_ms if view_ms > 0 else float('inf')

        print(f'{_fmt(copy_ms):>12s}  {_fmt(view_ms):>12s}  {speedup:>7.1f}x')

        results.append({
            'size': label, 'size_bytes': actual_bytes, 'n_rows': n_rows,
            'rounds': rounds, 'copy_ms': copy_ms, 'view_ms': view_ms,
            'speedup': speedup,
        })

        del buf
        gc.collect()

    print('=' * 78)
    print()
    print('Copy  = np.frombuffer(buf, dtype).copy() — view mode: buffer released,')
    print('        MUST copy to keep data alive.')
    print('View  = np.frombuffer(buf, dtype)         — hold mode: buffer stays alive,')
    print('        zero-copy columnar access.')

    # Write results
    results_dir = os.path.join(os.path.dirname(__file__), 'results')
    os.makedirs(results_dir, exist_ok=True)
    out_path = os.path.join(results_dir, 'hold_vs_view.txt')
    with open(out_path, 'w') as f:
        f.write('Hold vs View: Grid Numpy Benchmark\n')
        f.write(f'Grid dtype: {len(GRID_DTYPE)} columns, {ROW_SIZE} bytes/row\n')
        f.write(f'Columnar op: elevation.sum()\n\n')
        f.write(f'{"Size":>8s}\t{"Rows":>12s}\t{"Rounds":>6s}\t'
                f'{"Copy P50(ms)":>12s}\t{"View P50(ms)":>12s}\t{"Speedup":>8s}\n')
        for r in results:
            f.write(f'{r["size"]:>8s}\t{r["n_rows"]:>12d}\t{r["rounds"]:>6d}\t'
                    f'{r["copy_ms"]:>12.4f}\t{r["view_ms"]:>12.4f}\t'
                    f'{r["speedup"]:>7.1f}x\n')
    print(f'\nResults written to {os.path.abspath(out_path)}')


if __name__ == '__main__':
    main()
