"""Kostya-style coordinate benchmark — Ray actor counterpart.

Mirrors `kostya_ctwo_benchmark.py`:
  - Same workload: N coordinates × {x,y,z} f64
  - Client requests N → actor generates → client computes (mean_x, mean_y, mean_z)
  - Two payload representations:
      * `ray-records` — list of (x,y,z) tuples (record-oriented, slow path)
      * `ray-arrays`  — dict[xs|ys|zs → ndarray] (numpy-aware, Ray's sweet spot)

Notes:
  * Ray on macOS caps object_store_memory at 2 GiB. Cap N accordingly.
  * Run from the dedicated Ray venv:
        /tmp/ray_bench_env/bin/python sdk/python/benchmarks/kostya_ray_benchmark.py
"""
from __future__ import annotations

import gc
import math
import statistics
import time

import numpy as np
import ray


SIZES = [
    (1_000,      '1K'),
    (10_000,     '10K'),
    (100_000,    '100K'),
    (1_000_000,  '1M'),
    (3_000_000,  '3M'),
]

WARMUP = 3


def rounds_for(n: int) -> int:
    if n <= 10_000:
        return 100
    if n <= 100_000:
        return 30
    if n <= 1_000_000:
        return 15
    return 5


@ray.remote
class CoordSourceActor:
    def __init__(self):
        self._cache: dict[tuple[str, int], object] = {}

    def gen_records(self, n: int):
        key = ('records', n)
        if key not in self._cache:
            self._cache[key] = [
                {'row_id': i, 'x': i * 0.1, 'y': i * 0.2, 'z': i * 0.3,
                 'name': f'coord_{i % 50000:05d}'}
                for i in range(n)
            ]
        return self._cache[key]

    def gen_arrays(self, n: int):
        key = ('arrays', n)
        if key not in self._cache:
            idx = np.arange(n, dtype=np.uint32)
            self._cache[key] = {
                'row_id': idx,
                'x': idx.astype(np.float64) * 0.1,
                'y': idx.astype(np.float64) * 0.2,
                'z': idx.astype(np.float64) * 0.3,
                'name': [f'coord_{i % 50000:05d}' for i in range(n)],
            }
        return self._cache[key]


def consume_records(items) -> float:
    return sum(r['x'] + r['y'] + r['z'] for r in items)


def consume_arrays(d) -> float:
    return float(d['x'].sum() + d['y'].sum() + d['z'].sum())


def bench(actor, method_name: str, consumer, n: int, rounds: int):
    method = getattr(actor, method_name)

    for _ in range(WARMUP):
        consumer(ray.get(method.remote(n)))

    rpc_lat: list[float] = []
    total_lat: list[float] = []

    gc.disable()
    try:
        for _ in range(rounds):
            t0 = time.perf_counter()
            result = ray.get(method.remote(n))
            t1 = time.perf_counter()
            _ = consumer(result)
            t2 = time.perf_counter()
            del result
            rpc_lat.append(t1 - t0)
            total_lat.append(t2 - t0)
    finally:
        gc.enable()

    return (
        statistics.median(rpc_lat) * 1000.0,
        statistics.median(total_lat) * 1000.0,
        statistics.median([t - r for t, r in zip(total_lat, rpc_lat)]) * 1000.0,
    )


def main() -> None:
    ray.init(
        num_cpus=2,
        object_store_memory=2 * 1024 * 1024 * 1024,
        include_dashboard=False,
        ignore_reinit_error=True,
        logging_level='WARNING',
        _system_config={'automatic_object_spilling_enabled': False},
    )

    actor = CoordSourceActor.remote()
    ray.get(actor.gen_arrays.remote(10))  # warmup actor process

    print('=' * 92)
    print(f'Kostya-style coordinate benchmark — Ray {ray.__version__}')
    print(f'NumPy: {np.__version__}  Warmup: {WARMUP}  |  Adaptive rounds')
    print('=' * 92)
    print(f'{"N":>8}  {"Strategy":>16}  {"RPC P50":>10}  {"Aggregate":>10}  {"Total P50":>10}')
    print('-' * 92)

    rows: list[tuple[str, str, float, float, float]] = []
    for n, label in SIZES:
        skip_records = n >= 3_000_000
        rounds = rounds_for(n)

        if not skip_records:
            try:
                rpc, total, agg = bench(actor, 'gen_records', consume_records, n, rounds)
                print(f'{label:>8}  {"ray-records":>16}  '
                      f'{rpc:>9.3f}m  {agg:>9.3f}m  {total:>9.3f}m')
                rows.append((label, 'ray-records', rpc, total, agg))
            except Exception as e:  # noqa: BLE001
                print(f'{label:>8}  {"ray-records":>16}  FAILED: {e}')

        try:
            rpc, total, agg = bench(actor, 'gen_arrays', consume_arrays, n, rounds)
            print(f'{label:>8}  {"ray-arrays":>16}  '
                  f'{rpc:>9.3f}m  {agg:>9.3f}m  {total:>9.3f}m')
            rows.append((label, 'ray-arrays', rpc, total, agg))
        except Exception as e:  # noqa: BLE001
            print(f'{label:>8}  {"ray-arrays":>16}  FAILED: {e}')

        print()

    print('=' * 92)
    print('RPC P50    = ray.get of remote actor call')
    print('Aggregate  = client time to compute (mean_x, mean_y, mean_z) over received data')
    print('Total P50  = RPC + Aggregate')

    ray.shutdown()

    import os
    results_dir = os.path.join(os.path.dirname(__file__), 'results')
    os.makedirs(results_dir, exist_ok=True)
    out_path = os.path.join(results_dir, 'kostya_ray.txt')
    with open(out_path, 'w') as f:
        f.write('# Kostya-style coordinate benchmark — Ray\n')
        f.write(f'# Ray {ray.__version__}\n')
        f.write(f'{"N":>8}\t{"Strategy":>16}\t{"RPC_ms":>10}\t{"Total_ms":>10}\t{"Aggregate_ms":>14}\n')
        for label, strat, rpc, total, agg in rows:
            f.write(f'{label:>8}\t{strat:>16}\t{rpc:>10.4f}\t{total:>10.4f}\t{agg:>14.4f}\n')
    print(f'\nResults written to {out_path}')


if __name__ == '__main__':
    main()
