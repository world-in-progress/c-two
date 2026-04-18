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
        /tmp/ray_bench_env/bin/python benchmarks/kostya_ray_benchmark.py
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
    (10_000_000, '10M'),
]

WARMUP = 3


def rounds_for(n: int) -> int:
    if n <= 10_000:
        return 200
    if n <= 100_000:
        return 50
    if n <= 1_000_000:
        return 20
    return 5


@ray.remote
class CoordSourceActor:
    def gen_records(self, n: int, seed: int):
        rng = np.random.default_rng(seed)
        xs = rng.random(n).tolist()
        ys = rng.random(n).tolist()
        zs = rng.random(n).tolist()
        return list(zip(xs, ys, zs))

    def gen_arrays(self, n: int, seed: int):
        rng = np.random.default_rng(seed)
        return {
            'xs': rng.random(n),
            'ys': rng.random(n),
            'zs': rng.random(n),
        }


def consume_records(items) -> tuple[float, float, float]:
    n = len(items)
    sx = sy = sz = 0.0
    for x, y, z in items:
        sx += x; sy += y; sz += z
    return sx / n, sy / n, sz / n


def consume_arrays(d) -> tuple[float, float, float]:
    return float(d['xs'].mean()), float(d['ys'].mean()), float(d['zs'].mean())


def bench(actor, method_name: str, consumer, n: int, rounds: int):
    method = getattr(actor, method_name)

    for _ in range(WARMUP):
        consumer(ray.get(method.remote(n, 42)))

    rpc_lat: list[float] = []
    total_lat: list[float] = []

    gc.disable()
    try:
        for _ in range(rounds):
            t0 = time.perf_counter()
            result = ray.get(method.remote(n, 42))
            t1 = time.perf_counter()
            means = consumer(result)
            t2 = time.perf_counter()
            del result
            rpc_lat.append(t1 - t0)
            total_lat.append(t2 - t0)
    finally:
        gc.enable()

    assert all(0.4 < m < 0.6 for m in means), f'unexpected means: {means}'
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
    ray.get(actor.gen_arrays.remote(10, 0))  # warmup actor process

    print('=' * 92)
    print(f'Kostya-style coordinate benchmark — Ray {ray.__version__}')
    print(f'NumPy: {np.__version__}  Warmup: {WARMUP}  |  Adaptive rounds')
    print('=' * 92)
    print(f'{"N":>8}  {"Strategy":>16}  {"RPC P50":>10}  {"Aggregate":>10}  {"Total P50":>10}  {"GB/s":>8}')
    print('-' * 92)

    rows: list[tuple[str, str, float, float, float]] = []
    for n, label in SIZES:
        # Skip records at very large N — turns into multi-second territory.
        skip_records = n >= 10_000_000
        rounds = rounds_for(n)
        size_bytes = n * 24

        if not skip_records:
            try:
                rpc, total, agg = bench(actor, 'gen_records', consume_records, n, rounds)
                tput = (size_bytes / (total / 1000.0)) / (1024 ** 3)
                print(f'{label:>8}  {"ray-records":>16}  '
                      f'{rpc:>9.3f}m  {agg:>9.3f}m  {total:>9.3f}m  {tput:>6.2f}')
                rows.append((label, 'ray-records', rpc, total, agg))
            except Exception as e:  # noqa: BLE001
                print(f'{label:>8}  {"ray-records":>16}  FAILED: {e}')

        try:
            rpc, total, agg = bench(actor, 'gen_arrays', consume_arrays, n, rounds)
            tput = (size_bytes / (total / 1000.0)) / (1024 ** 3)
            print(f'{label:>8}  {"ray-arrays":>16}  '
                  f'{rpc:>9.3f}m  {agg:>9.3f}m  {total:>9.3f}m  {tput:>6.2f}')
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
