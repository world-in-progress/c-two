"""Kostya-style benchmark for c-two: coordinates round-trip + columnar aggregate.

Inspired by the canonical kostya `json` benchmark
(https://github.com/kostya/benchmarks/tree/master/json) — generate N
`{x: f64, y: f64, z: f64}` coordinates, transport them to a remote consumer,
then compute `(mean_x, mean_y, mean_z)` so deserialization cost is observable.

This script compares three transferable strategies over the **same** c-two IPC
transport, isolating serialization cost from RPC cost:

  1. `pickle-records`  — pickle of `list[Coordinate]` (naive Python objects)
  2. `pickle-arrays`   — pickle of `{xs|ys|zs: np.ndarray}` (numpy-aware pickle)
  3. `fastdb-hold`     — fastdb `Feature` with `from_buffer` + hold mode
                         (zero-deserialization, columnar SHM view)

Companion: `kostya_ray_benchmark.py` runs the equivalent through Ray actors
for cross-framework comparison.

Usage:
    C2_RELAY_ADDRESS= uv run python benchmarks/kostya_ctwo_benchmark.py
    C2_RELAY_ADDRESS= uv run python benchmarks/kostya_ctwo_benchmark.py --max-n 1000000
"""
from __future__ import annotations

import argparse
import gc
import math
import os
import pickle
import statistics
import sys
import time

import numpy as np

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))

import c_two as cc

import fastdb4py as fx


# ---------------------------------------------------------------------------
# Workload sizing — N coordinates × 24 bytes/record (3 × f64)
# ---------------------------------------------------------------------------

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


# ---------------------------------------------------------------------------
# Variant 1 — pickle of list[Coordinate] (record-oriented, the slow case)
# ---------------------------------------------------------------------------

@cc.transferable
class CoordRecords:
    n: int
    seed: int

    def serialize(self: 'CoordRecords') -> bytes:
        return pickle.dumps((self.n, self.seed))

    def deserialize(raw: bytes) -> 'CoordRecords':
        n, seed = pickle.loads(bytes(raw) if isinstance(raw, memoryview) else raw)
        return CoordRecords(n=n, seed=seed)


@cc.transferable
class CoordList:
    """list[(x,y,z)] — pickled as a list of tuples; consumer iterates in Python."""
    items: list  # list[tuple[float, float, float]]

    def serialize(self: 'CoordList') -> bytes:
        return pickle.dumps(self.items, protocol=5)

    def deserialize(raw: bytes) -> 'CoordList':
        return CoordList(items=pickle.loads(bytes(raw) if isinstance(raw, memoryview) else raw))


# ---------------------------------------------------------------------------
# Variant 2 — pickle of dict[str, ndarray] (numpy-aware, fast case)
# ---------------------------------------------------------------------------

@cc.transferable
class CoordArrays:
    """{xs, ys, zs} as numpy arrays; pickle is already numpy-aware."""
    xs: np.ndarray
    ys: np.ndarray
    zs: np.ndarray

    def serialize(self: 'CoordArrays') -> bytes:
        return pickle.dumps({'xs': self.xs, 'ys': self.ys, 'zs': self.zs}, protocol=5)

    def deserialize(raw: bytes) -> 'CoordArrays':
        d = pickle.loads(bytes(raw) if isinstance(raw, memoryview) else raw)
        return CoordArrays(xs=d['xs'], ys=d['ys'], zs=d['zs'])


# ---------------------------------------------------------------------------
# Variant 3 — fastdb Feature with from_buffer (zero-deserialization, hold mode)
# ---------------------------------------------------------------------------

class CoordFeature(fx.Feature):
    xs: np.ndarray
    ys: np.ndarray
    zs: np.ndarray


@cc.transferable
class CoordFastdb:
    """Wraps a fastdb Feature; from_buffer parses directly out of an SHM view.

    Triggers c-two's auto hold-mode (transferable.py:518-520) — the SHM
    backing the response is held alive until the client releases it.
    """
    feat: object  # CoordFeature; declared as object to keep dataclass happy

    def serialize(self: 'CoordFastdb') -> bytes:
        return fx.FastSerializer.dumps(self.feat)

    def deserialize(raw: bytes) -> 'CoordFastdb':
        feat = fx.FastSerializer.loads(
            bytes(raw) if isinstance(raw, memoryview) else raw,
            CoordFeature,
        )
        return CoordFastdb(feat=feat)

    def from_buffer(buf: memoryview) -> 'CoordFastdb':
        # fastdb.loads accepts buffer-protocol objects without copying.
        feat = fx.FastSerializer.loads(buf, CoordFeature)
        return CoordFastdb(feat=feat)


# ---------------------------------------------------------------------------
# CRM contract — three methods, one per transferable variant
# ---------------------------------------------------------------------------

@cc.crm(namespace='bench.kostya', version='0.1.0')
class CoordSource:
    def gen_records(self, req: CoordRecords) -> CoordList: ...
    def gen_arrays(self, req: CoordRecords) -> CoordArrays: ...
    # Output has from_buffer → cc.hold() at call site picks the zero-copy path.
    def gen_fastdb(self, req: CoordRecords) -> CoordFastdb: ...


class CoordSourceImpl:
    def gen_records(self, req: CoordRecords) -> CoordList:
        rng = np.random.default_rng(req.seed)
        # Generate columnar then convert to list of tuples — list-of-records
        # is the slow representation we want to measure.
        xs = rng.random(req.n)
        ys = rng.random(req.n)
        zs = rng.random(req.n)
        items = list(zip(xs.tolist(), ys.tolist(), zs.tolist()))
        return CoordList(items=items)

    def gen_arrays(self, req: CoordRecords) -> CoordArrays:
        rng = np.random.default_rng(req.seed)
        return CoordArrays(
            xs=rng.random(req.n),
            ys=rng.random(req.n),
            zs=rng.random(req.n),
        )

    def gen_fastdb(self, req: CoordRecords) -> CoordFastdb:
        rng = np.random.default_rng(req.seed)
        feat = CoordFeature(
            xs=rng.random(req.n),
            ys=rng.random(req.n),
            zs=rng.random(req.n),
        )
        return CoordFastdb(feat=feat)


# ---------------------------------------------------------------------------
# Consumers — same final aggregate (mean_x, mean_y, mean_z)
# ---------------------------------------------------------------------------

def consume_list(result: CoordList) -> tuple[float, float, float]:
    """Pure-Python aggregate over list-of-tuples — the slow path."""
    items = result.items
    n = len(items)
    sx = sy = sz = 0.0
    for x, y, z in items:
        sx += x; sy += y; sz += z
    return sx / n, sy / n, sz / n


def consume_arrays(result: CoordArrays) -> tuple[float, float, float]:
    return float(result.xs.mean()), float(result.ys.mean()), float(result.zs.mean())


def consume_fastdb(result: CoordFastdb) -> tuple[float, float, float]:
    feat = result.feat
    return float(feat.xs.mean()), float(feat.ys.mean()), float(feat.zs.mean())


# ---------------------------------------------------------------------------
# IPC server bootstrap — single per-process server, three CRMs share it
# ---------------------------------------------------------------------------

_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')


def _wait_for_socket(address: str, timeout: float = 5.0) -> None:
    region_id = address.replace('ipc://', '')
    sock_path = os.path.join(_IPC_SOCK_DIR, f'{region_id}.sock')
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.exists(sock_path):
            return
        time.sleep(0.05)


# ---------------------------------------------------------------------------
# Benchmark runners
# ---------------------------------------------------------------------------

def bench_variant(proxy, method_name: str, consumer, n: int, rounds: int,
                  use_hold: bool) -> tuple[float, float, float]:
    req = CoordRecords(n=n, seed=42)

    method = getattr(proxy, method_name)

    # Warmup
    for _ in range(WARMUP):
        if use_hold:
            with cc.hold(method)(req) as held:
                consumer(held.value)
        else:
            consumer(method(req))

    rpc_lat: list[float] = []      # round-trip + serialization-only
    total_lat: list[float] = []    # round-trip + consume (aggregate)

    gc.disable()
    try:
        for _ in range(rounds):
            t0 = time.perf_counter()
            if use_hold:
                with cc.hold(method)(req) as held:
                    t1 = time.perf_counter()
                    means = consumer(held.value)
                    t2 = time.perf_counter()
            else:
                result = method(req)
                t1 = time.perf_counter()
                means = consumer(result)
                t2 = time.perf_counter()
                del result
            rpc_lat.append(t1 - t0)
            total_lat.append(t2 - t0)
    finally:
        gc.enable()

    # Sanity check
    assert all(0.4 < m < 0.6 for m in means), f'unexpected means: {means}'

    return (
        statistics.median(rpc_lat) * 1000.0,
        statistics.median(total_lat) * 1000.0,
        statistics.median([t - r for t, r in zip(total_lat, rpc_lat)]) * 1000.0,
    )


def main() -> None:
    parser = argparse.ArgumentParser(description='Kostya-style coordinate benchmark for c-two')
    parser.add_argument('--max-n', type=int, default=None, help='Cap maximum N')
    parser.add_argument('--skip-records', action='store_true',
                        help='Skip the slow list-of-records variant (huge at 10M)')
    args = parser.parse_args()

    sizes = [(n, lbl) for n, lbl in SIZES if args.max_n is None or n <= args.max_n]

    cc.set_server(segment_size=2 * 1024 * 1024 * 1024, max_segments=8)
    cc.set_client(segment_size=2 * 1024 * 1024 * 1024, max_segments=8)
    cc.register(CoordSource, CoordSourceImpl(), name='coord')
    address = cc.server_address()
    _wait_for_socket(address)

    proxy = cc.connect(CoordSource, name='coord', address=address)

    print('=' * 92)
    print('Kostya-style coordinate benchmark — C-Two IPC, three transferable strategies')
    print(f'NumPy: {np.__version__}  fastdb4py: present  Python: {sys.version.split()[0]}')
    print(f'Warmup: {WARMUP}  |  Adaptive rounds')
    print('=' * 92)
    print(f'{"N":>8}  {"Strategy":>16}  {"RPC P50":>10}  {"Aggregate":>10}  {"Total P50":>10}  {"GB/s":>8}')
    print('-' * 92)

    rows: list[tuple[str, str, float, float, float]] = []

    for n, label in sizes:
        rounds = rounds_for(n)
        size_bytes = n * 24  # 3 × f64

        # Variant 1 — pickle of list-of-records
        if not (args.skip_records and n >= 1_000_000):
            try:
                rpc, total, agg = bench_variant(
                    proxy, 'gen_records', consume_list, n, rounds, use_hold=False,
                )
                tput = (size_bytes / (total / 1000.0)) / (1024 ** 3)
                print(f'{label:>8}  {"pickle-records":>16}  '
                      f'{rpc:>9.3f}m  {agg:>9.3f}m  {total:>9.3f}m  {tput:>6.2f}')
                rows.append((label, 'pickle-records', rpc, total, agg))
            except Exception as e:  # noqa: BLE001
                print(f'{label:>8}  {"pickle-records":>16}  FAILED: {e}')

        # Variant 2 — pickle of dict-of-arrays (numpy-aware)
        try:
            rpc, total, agg = bench_variant(
                proxy, 'gen_arrays', consume_arrays, n, rounds, use_hold=False,
            )
            tput = (size_bytes / (total / 1000.0)) / (1024 ** 3)
            print(f'{label:>8}  {"pickle-arrays":>16}  '
                  f'{rpc:>9.3f}m  {agg:>9.3f}m  {total:>9.3f}m  {tput:>6.2f}')
            rows.append((label, 'pickle-arrays', rpc, total, agg))
        except Exception as e:  # noqa: BLE001
            print(f'{label:>8}  {"pickle-arrays":>16}  FAILED: {e}')

        # Variant 3 — fastdb + hold mode (zero-deser)
        try:
            rpc, total, agg = bench_variant(
                proxy, 'gen_fastdb', consume_fastdb, n, rounds, use_hold=True,
            )
            tput = (size_bytes / (total / 1000.0)) / (1024 ** 3)
            print(f'{label:>8}  {"fastdb-hold":>16}  '
                  f'{rpc:>9.3f}m  {agg:>9.3f}m  {total:>9.3f}m  {tput:>6.2f}')
            rows.append((label, 'fastdb-hold', rpc, total, agg))
        except Exception as e:  # noqa: BLE001
            print(f'{label:>8}  {"fastdb-hold":>16}  FAILED: {e}')

        print()

    print('=' * 92)
    print('RPC P50    = client wall time, request → result object received')
    print('Aggregate  = client time to compute (mean_x, mean_y, mean_z) over received data')
    print('Total P50  = RPC + Aggregate (end-to-end work-completed latency)')
    print('GB/s       = throughput based on raw payload size (24 bytes × N)')

    cc.close(proxy)
    cc.unregister('coord')
    cc.shutdown()

    # Persist results
    results_dir = os.path.join(os.path.dirname(__file__), 'results')
    os.makedirs(results_dir, exist_ok=True)
    out_path = os.path.join(results_dir, 'kostya_ctwo.txt')
    with open(out_path, 'w') as f:
        f.write('# Kostya-style coordinate benchmark — C-Two IPC\n')
        f.write(f'# {len(rows)} measurements\n')
        f.write(f'{"N":>8}\t{"Strategy":>16}\t{"RPC_ms":>10}\t{"Total_ms":>10}\t{"Aggregate_ms":>14}\n')
        for label, strat, rpc, total, agg in rows:
            f.write(f'{label:>8}\t{strat:>16}\t{rpc:>10.4f}\t{total:>10.4f}\t{agg:>14.4f}\n')
    print(f'\nResults written to {out_path}')


def _geomean(xs: list[float]) -> float:
    return math.exp(sum(math.log(x) for x in xs) / len(xs))


if __name__ == '__main__':
    main()
