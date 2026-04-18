"""Kostya-style coordinate benchmark — c-two IPC variants.

Compares three transferable strategies for transporting a list of coordinate
records (matches fastdb's own kostya benchmark schema: row_id, x, y, z, name).

Strategies:
  * pickle-records  : list[dict] over pickle (kostya canonical, slow)
  * pickle-arrays   : 4 numpy arrays over pickle (numpy fast path)
  * fastdb-hold     : fastdb ORM raw buffer + zero-copy load_xbuffer in hold mode

Run:
    C2_RELAY_ADDRESS= uv run python benchmarks/kostya_ctwo_benchmark.py \
        --variant fastdb-hold --n 1000000 --iters 30
"""
from __future__ import annotations

import argparse
import multiprocessing as mp
import os
import pickle
import statistics
import time
from typing import Callable

import numpy as np

import c_two as cc

# ---------------------------------------------------------------------------
# fastdb is optional; only required for the fastdb-hold variant
# ---------------------------------------------------------------------------
try:
    from fastdb4py import core, ORM, F64, U32, STR, Feature

    class Coord(Feature):
        row_id: U32
        x: F64
        y: F64
        z: F64
        name: STR

    HAS_FASTDB = True
except ImportError:
    HAS_FASTDB = False
    Coord = None  # type: ignore


# ---------------------------------------------------------------------------
# Transferables — one per wire-format strategy
# ---------------------------------------------------------------------------

@cc.transferable
class CoordRecords:
    """Pickle of list[dict] — kostya canonical, brutal on per-row overhead."""
    records: list

    def serialize(data: 'CoordRecords') -> bytes:
        return pickle.dumps(data.records, protocol=pickle.HIGHEST_PROTOCOL)

    def deserialize(buf: bytes) -> 'CoordRecords':
        return CoordRecords(records=pickle.loads(buf))


@cc.transferable
class CoordArrays:
    """Pickle of 4 numpy arrays + 1 string list — numpy fast path."""
    row_id: np.ndarray
    x: np.ndarray
    y: np.ndarray
    z: np.ndarray
    name: list

    def serialize(data: 'CoordArrays') -> bytes:
        return pickle.dumps(
            (data.row_id, data.x, data.y, data.z, data.name),
            protocol=pickle.HIGHEST_PROTOCOL,
        )

    def deserialize(buf: bytes) -> 'CoordArrays':
        row_id, x, y, z, name = pickle.loads(buf)
        return CoordArrays(row_id=row_id, x=x, y=y, z=z, name=name)


if HAS_FASTDB:
    @cc.transferable
    class CoordFastdb:
        """Wraps a fastdb ORM and exposes its raw columnar buffer.

        The whole point: ``from_buffer(memoryview)`` calls
        ``WxDatabase.load_xbuffer`` directly on c-two's hold-mode SHM view —
        zero copy, zero parse, just a header walk in C++.
        """
        orm: object  # ORM (mutable side) or memoryview-backed ORM (consumer side)

        def serialize(data: 'CoordFastdb') -> bytes:
            buf = data.orm._origin.buffer().as_array(np.uint8)
            return bytes(buf)

        def deserialize(buf: bytes) -> 'CoordFastdb':
            db = core.WxDatabase.load_xbuffer(memoryview(buf))
            orm = ORM()
            orm._origin = db
            for i in range(db.get_layer_count()):
                tbl = db.get_layer(i)
                if tbl.name() == '_name_':
                    orm._named_table = tbl
                    break
            return CoordFastdb(orm=orm)

        def from_buffer(buf: memoryview) -> 'CoordFastdb':
            db = core.WxDatabase.load_xbuffer(buf)
            orm = ORM()
            orm._origin = db
            for i in range(db.get_layer_count()):
                tbl = db.get_layer(i)
                if tbl.name() == '_name_':
                    orm._named_table = tbl
                    break
            return CoordFastdb(orm=orm)


# ---------------------------------------------------------------------------
# CRM contracts — one per variant (must match what server registers)
# ---------------------------------------------------------------------------

@cc.crm(namespace='bench.kostya', version='0.1.0')
class ICoordRecords:
    def echo(self, data: CoordRecords) -> CoordRecords: ...


@cc.crm(namespace='bench.kostya', version='0.1.0')
class ICoordArrays:
    def echo(self, data: CoordArrays) -> CoordArrays: ...


if HAS_FASTDB:
    @cc.crm(namespace='bench.kostya', version='0.1.0')
    class ICoordFastdb:
        def echo(self, data: CoordFastdb) -> CoordFastdb: ...


# ---------------------------------------------------------------------------
# CRM implementations — pre-build the payload, return on each call
# ---------------------------------------------------------------------------

class CoordRecordsCRM:
    def __init__(self, n: int):
        self._payload = CoordRecords(records=[
            {'row_id': i, 'x': i * 0.1, 'y': i * 0.2, 'z': i * 0.3,
             'name': f'coord_{i % 50000:05d}'}
            for i in range(n)
        ])

    def echo(self, data: CoordRecords) -> CoordRecords:
        return self._payload


class CoordArraysCRM:
    def __init__(self, n: int):
        idx = np.arange(n, dtype=np.uint32)
        self._payload = CoordArrays(
            row_id=idx,
            x=idx.astype(np.float64) * 0.1,
            y=idx.astype(np.float64) * 0.2,
            z=idx.astype(np.float64) * 0.3,
            name=[f'coord_{i % 50000:05d}' for i in range(n)],
        )

    def echo(self, data: CoordArrays) -> CoordArrays:
        return self._payload


if HAS_FASTDB:
    class CoordFastdbCRM:
        def __init__(self, n: int):
            orm = ORM.create()
            for i in range(n):
                f = Coord()
                f.row_id = i
                f.x = i * 0.1
                f.y = i * 0.2
                f.z = i * 0.3
                f.name = f'coord_{i % 50000:05d}'
                orm.push(f)
            orm._combine()
            self._payload = CoordFastdb(orm=orm)

        def echo(self, data: CoordFastdb) -> CoordFastdb:
            return self._payload


# ---------------------------------------------------------------------------
# Consumer-side aggregations — what we measure includes both transport AND
# the cost of materializing usable values from the received representation
# ---------------------------------------------------------------------------

def consume_records(payload: CoordRecords) -> float:
    return sum(r['x'] + r['y'] + r['z'] for r in payload.records)


def consume_arrays(payload: CoordArrays) -> float:
    return float(payload.x.sum() + payload.y.sum() + payload.z.sum())


def consume_fastdb(payload) -> float:
    tbl = payload.orm[Coord][Coord]
    cx = tbl.column.x
    cy = tbl.column.y
    cz = tbl.column.z
    return float(cx[:].sum() + cy[:].sum() + cz[:].sum())


# ---------------------------------------------------------------------------
# Server / client harness
# ---------------------------------------------------------------------------

VARIANTS = {
    'pickle-records': (ICoordRecords, CoordRecordsCRM, consume_records),
    'pickle-arrays':  (ICoordArrays,  CoordArraysCRM,  consume_arrays),
}
if HAS_FASTDB:
    VARIANTS['fastdb-hold'] = (ICoordFastdb, CoordFastdbCRM, consume_fastdb)


def _server_main(variant: str, n: int, ready_path: str) -> None:
    icrm_cls, crm_cls, _ = VARIANTS[variant]
    cc.set_server(pool_segment_size=2 * 1024 * 1024 * 1024, max_pool_segments=8)
    cc.register(icrm_cls, crm_cls(n), name='coords')
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


def run_variant(variant: str, n: int, iters: int, warmup: int) -> dict:
    icrm_cls, _, consumer = VARIANTS[variant]

    ready_path = f'/tmp/kostya_{variant}_{os.getpid()}_{time.time_ns()}.ready'

    ctx = mp.get_context('spawn')
    proc = ctx.Process(target=_server_main, args=(variant, n, ready_path))
    proc.start()
    try:
        address = _wait_ready(ready_path)

        cc.set_client(pool_segment_size=2 * 1024 * 1024 * 1024, max_pool_segments=8)
        proxy = cc.connect(icrm_cls, name='coords', address=address)

        # Warmup
        for _ in range(warmup):
            with cc.hold(proxy.echo)(_dummy_for_variant(variant, 1)) as held:
                _ = consumer(held.value)

        # Measure end-to-end: call + materialize + columnar read
        total_ms: list[float] = []
        for _ in range(iters):
            t0 = time.perf_counter()
            with cc.hold(proxy.echo)(_dummy_for_variant(variant, 1)) as held:
                _ = consumer(held.value)
            total_ms.append((time.perf_counter() - t0) * 1000)

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
        try: os.unlink(ready_path)
        except FileNotFoundError: pass


def _dummy_for_variant(variant: str, n: int):
    """Tiny request payload — what the client sends; server ignores it."""
    if variant == 'pickle-records':
        return CoordRecords(records=[{'row_id': 0, 'x': 0.0, 'y': 0.0, 'z': 0.0, 'name': 'x'}] * n)
    if variant == 'pickle-arrays':
        z = np.zeros(n, dtype=np.uint32)
        zf = np.zeros(n, dtype=np.float64)
        return CoordArrays(row_id=z, x=zf, y=zf, z=zf, name=['x'] * n)
    if variant == 'fastdb-hold':
        orm = ORM.create()
        f = Coord(); f.row_id = 0; f.x = 0.0; f.y = 0.0; f.z = 0.0; f.name = 'x'
        orm.push(f)
        orm._combine()
        return CoordFastdb(orm=orm)
    raise ValueError(variant)


# ---------------------------------------------------------------------------

def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument('--variant', choices=sorted(VARIANTS.keys()), required=True)
    p.add_argument('--n', type=int, required=True, help='record count')
    p.add_argument('--iters', type=int, default=30)
    p.add_argument('--warmup', type=int, default=3)
    args = p.parse_args()

    res = run_variant(args.variant, args.n, args.iters, args.warmup)
    print(f"\n=== c-two IPC | variant={res['variant']} N={res['n']:,} iters={res['iters']} ===")
    print(f"  p10  {res['p10_ms']:8.2f} ms")
    print(f"  p50  {res['p50_ms']:8.2f} ms")
    print(f"  p90  {res['p90_ms']:8.2f} ms")
    print(f"  mean {res['mean_ms']:8.2f} ms  (min {res['min_ms']:.2f} / max {res['max_ms']:.2f})")


if __name__ == '__main__':
    main()
