"""Kostya-style coordinate benchmark — C-Two IPC variants.

Compares Python pickle fallback with the current FastDB-first CRM ABI and
client-side ``cc.hold(...)`` model for transporting coordinate records:
``row_id, x, y, z, name``.

Strategies:
  * pickle-records       : list[dict] over Python pickle
  * pickle-arrays        : numpy arrays + string list over Python pickle
  * fastdb-normal        : fdb.Batch[Coord] normal call, owned-buffer safe columns
  * fastdb-hold          : fdb.Batch[Coord] held response, safe owner-checked columns
  * fastdb-hold-unsafe   : fdb.Batch[Coord] held response, explicit unsafe numpy views

Run:
    C2_RELAY_ANCHOR_ADDRESS= uv run python sdk/python/benchmarks/kostya_ctwo_benchmark.py \
        --variant fastdb-hold --n 100000 --iters 20
"""
from __future__ import annotations

import argparse
import gc
import multiprocessing as mp
import os
import statistics
import sys
import time
from collections.abc import Callable
from dataclasses import dataclass

import fastdb4py as fdb
import numpy as np

import c_two as cc


_BENCH_SCHEMA_MODULE = 'c_two_kostya_benchmark'
sys.modules.setdefault(_BENCH_SCHEMA_MODULE, sys.modules[__name__])


@fdb.feature
class Coord:
    row_id: fdb.U32
    x: fdb.F64
    y: fdb.F64
    z: fdb.F64
    name: fdb.STR


Coord.__module__ = _BENCH_SCHEMA_MODULE


@dataclass
class CoordRecords:
    """Pickle of list[dict] — Kostya canonical, row-oriented path."""

    records: list[dict[str, object]]


CoordRecords.__module__ = _BENCH_SCHEMA_MODULE


@dataclass
class CoordArrays:
    """Pickle of column arrays plus names — numpy-friendly fallback path."""

    row_id: np.ndarray
    x: np.ndarray
    y: np.ndarray
    z: np.ndarray
    name: list[str]


CoordArrays.__module__ = _BENCH_SCHEMA_MODULE


@cc.crm(namespace='bench.kostya.pickle.records', version='0.1.0')
class ICoordRecords:
    def coords(self, count: int) -> CoordRecords:
        ...


@cc.crm(namespace='bench.kostya.pickle.arrays', version='0.1.0')
class ICoordArrays:
    def coords(self, count: int) -> CoordArrays:
        ...


@cc.crm(namespace='bench.kostya.fastdb', version='0.1.0')
class ICoordFastdb:
    def coords(self, count: fdb.I32) -> fdb.Batch[Coord]:
        ...


def _coord_names(n: int) -> list[str]:
    return [f'coord_{i % 50000:05d}' for i in range(n)]


def _coord_index(n: int) -> np.ndarray:
    return np.arange(n, dtype=np.uint32)


def _make_coord_arrays(n: int) -> CoordArrays:
    idx = _coord_index(n)
    return CoordArrays(
        row_id=idx,
        x=idx.astype(np.float64) * 0.1,
        y=idx.astype(np.float64) * 0.2,
        z=idx.astype(np.float64) * 0.3,
        name=_coord_names(n),
    )


def _make_coord_table(n: int) -> fdb.Table[Coord]:
    idx = _coord_index(n)
    engine = fdb.ColumnEngine.truncate([fdb.Layout(Coord, n, name='return_0')])
    table = engine.table(Coord, name='return_0')
    table.fill(
        row_id=idx,
        x=idx.astype(np.float64) * 0.1,
        y=idx.astype(np.float64) * 0.2,
        z=idx.astype(np.float64) * 0.3,
        name=_coord_names(n),
    )
    return table


class CoordRecordsCRM:
    def __init__(self, n: int):
        self._payload = CoordRecords(records=[
            {
                'row_id': i,
                'x': i * 0.1,
                'y': i * 0.2,
                'z': i * 0.3,
                'name': f'coord_{i % 50000:05d}',
            }
            for i in range(n)
        ])

    def coords(self, count: int) -> CoordRecords:
        return self._payload


class CoordArraysCRM:
    def __init__(self, n: int):
        self._payload = _make_coord_arrays(n)

    def coords(self, count: int) -> CoordArrays:
        return self._payload


class CoordFastdbCRM:
    def __init__(self, n: int):
        self._payload = _make_coord_table(n)

    def coords(self, count: fdb.I32) -> fdb.Batch[Coord]:
        return self._payload


def consume_records(payload: CoordRecords) -> float:
    return sum(
        float(row['x']) + float(row['y']) + float(row['z'])
        for row in payload.records
    )


def consume_arrays(payload: CoordArrays) -> float:
    return float(payload.x.sum() + payload.y.sum() + payload.z.sum())


def consume_fastdb_safe(table: fdb.Table[Coord]) -> float:
    column = table.column
    return float(
        np.asarray(column.x).sum()
        + np.asarray(column.y).sum()
        + np.asarray(column.z).sum()
    )


def consume_fastdb_unsafe(table: fdb.Table[Coord]) -> float:
    column = table.column
    return float(
        column.x.unsafe_numpy_view().sum()
        + column.y.unsafe_numpy_view().sum()
        + column.z.unsafe_numpy_view().sum()
    )


def _request_count(_n: int) -> int:
    return 1


def _expected_total(n: int) -> float:
    return 0.6 * n * (n - 1) / 2


def _assert_expected_total(total: float, n: int) -> None:
    expected = _expected_total(n)
    if not np.isclose(total, expected, rtol=1e-9, atol=1e-6):
        raise AssertionError(
            f'payload validation failed: expected aggregate {expected}, got {total}',
        )


@dataclass(frozen=True)
class VariantSpec:
    contract: type
    resource_factory: Callable[[int], object]
    consumer: Callable[[object], float]
    use_hold: bool


VARIANTS: dict[str, VariantSpec] = {
    'pickle-records': VariantSpec(
        contract=ICoordRecords,
        resource_factory=CoordRecordsCRM,
        consumer=consume_records,
        use_hold=False,
    ),
    'pickle-arrays': VariantSpec(
        contract=ICoordArrays,
        resource_factory=CoordArraysCRM,
        consumer=consume_arrays,
        use_hold=False,
    ),
    'fastdb-normal': VariantSpec(
        contract=ICoordFastdb,
        resource_factory=CoordFastdbCRM,
        consumer=consume_fastdb_safe,
        use_hold=False,
    ),
    'fastdb-hold': VariantSpec(
        contract=ICoordFastdb,
        resource_factory=CoordFastdbCRM,
        consumer=consume_fastdb_safe,
        use_hold=True,
    ),
    'fastdb-hold-unsafe': VariantSpec(
        contract=ICoordFastdb,
        resource_factory=CoordFastdbCRM,
        consumer=consume_fastdb_unsafe,
        use_hold=True,
    ),
}


def _server_main(variant: str, n: int, ready_path: str) -> None:
    spec = VARIANTS[variant]
    cc.set_server(ipc_overrides={
        'pool_segment_size': 2 * 1024 * 1024 * 1024,
        'max_pool_segments': 8,
    })
    cc.register(spec.contract, spec.resource_factory(n), name='coords')
    addr = cc.server_address() or ''
    with open(ready_path, 'w') as f:
        f.write(addr)
    cc.serve()


def _wait_ready(ready_path: str, timeout: float = 60.0) -> str:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if os.path.exists(ready_path):
            with open(ready_path) as f:
                addr = f.read().strip()
            if addr:
                return addr
        time.sleep(0.05)
    raise TimeoutError(f'server not ready in {timeout}s')


def _call_once(method, spec: VariantSpec, consumer: Callable[[object], float], request: int) -> float:
    if spec.use_hold:
        with cc.hold(method)(request) as held:
            return consumer(held.value)
    return consumer(method(request))


def run_variant(variant: str, n: int, iters: int, warmup: int) -> dict[str, object]:
    spec = VARIANTS[variant]
    ready_path = f'/tmp/kostya_{variant}_{os.getpid()}_{time.time_ns()}.ready'

    ctx = mp.get_context('spawn')
    proc = ctx.Process(target=_server_main, args=(variant, n, ready_path))
    proc.start()
    try:
        address = _wait_ready(ready_path)

        cc.set_client(ipc_overrides={
            'pool_segment_size': 2 * 1024 * 1024 * 1024,
            'max_pool_segments': 8,
        })
        proxy = cc.connect(spec.contract, name='coords', address=address)
        method = proxy.coords
        request = _request_count(n)

        for _ in range(warmup):
            _assert_expected_total(
                _call_once(method, spec, spec.consumer, request),
                n,
            )

        total_ms: list[float] = []
        gc.disable()
        try:
            for _ in range(iters):
                t0 = time.perf_counter()
                _assert_expected_total(
                    _call_once(method, spec, spec.consumer, request),
                    n,
                )
                total_ms.append((time.perf_counter() - t0) * 1000)
        finally:
            gc.enable()

        cc.close(proxy)
        cc.shutdown()

        return {
            'variant': variant,
            'n': n,
            'iters': iters,
            'p50_ms': statistics.median(total_ms),
            'p10_ms': statistics.quantiles(total_ms, n=10)[0] if iters >= 10 else min(total_ms),
            'p90_ms': statistics.quantiles(total_ms, n=10)[8] if iters >= 10 else max(total_ms),
            'min_ms': min(total_ms),
            'max_ms': max(total_ms),
            'mean_ms': statistics.fmean(total_ms),
        }
    finally:
        proc.terminate()
        proc.join(timeout=5)
        try:
            os.unlink(ready_path)
        except FileNotFoundError:
            pass


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument('--variant', choices=sorted(VARIANTS.keys()), required=True)
    parser.add_argument('--n', type=int, required=True, help='record count')
    parser.add_argument('--iters', type=int, default=30)
    parser.add_argument('--warmup', type=int, default=3)
    args = parser.parse_args()

    res = run_variant(args.variant, args.n, args.iters, args.warmup)
    print(f"\n=== c-two IPC | variant={res['variant']} N={res['n']:,} iters={res['iters']} ===")
    print(f"  p10  {res['p10_ms']:8.2f} ms")
    print(f"  p50  {res['p50_ms']:8.2f} ms")
    print(f"  p90  {res['p90_ms']:8.2f} ms")
    print(f"  mean {res['mean_ms']:8.2f} ms  (min {res['min_ms']:.2f} / max {res['max_ms']:.2f})")


if __name__ == '__main__':
    main()
