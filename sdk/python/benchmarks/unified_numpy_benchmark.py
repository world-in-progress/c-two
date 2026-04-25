"""Unified NumPy IPC benchmark — C-Two vs Ray.

Both frameworks transport the same payload: 4 numpy arrays (row_id u32,
x/y/z f64) + 1 string list.  Measures end-to-end latency including
cross-process RPC + client-side aggregation.

Requires: numpy, ray, c-two  (run from a Python 3.12/3.13 venv)

Usage:
    python benchmarks/unified_numpy_benchmark.py
    python benchmarks/unified_numpy_benchmark.py --sizes 1000 10000 100000 1000000
"""
from __future__ import annotations

import argparse
import gc
import multiprocessing as mp
import os
import pickle
import statistics
import sys
import time

import numpy as np
import ray

import c_two as cc


# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------

WARMUP = 3

DEFAULT_SIZES = [
    (1_000, '1K'),
    (10_000, '10K'),
    (100_000, '100K'),
    (1_000_000, '1M'),
    (3_000_000, '3M'),
]


def rounds_for(n: int) -> int:
    if n <= 10_000:
        return 100
    if n <= 100_000:
        return 30
    if n <= 1_000_000:
        return 15
    return 5


def make_arrays(n: int):
    idx = np.arange(n, dtype=np.uint32)
    return {
        'row_id': idx,
        'x': idx.astype(np.float64) * 0.1,
        'y': idx.astype(np.float64) * 0.2,
        'z': idx.astype(np.float64) * 0.3,
        'name': [f'coord_{i % 50000:05d}' for i in range(n)],
    }


def consume(d) -> float:
    return float(d['x'].sum() + d['y'].sum() + d['z'].sum())


# Structured dtype for hold-mode zero-copy (numeric columns only)
COORD_DTYPE = np.dtype([
    ('row_id', np.uint32),
    ('x', np.float64),
    ('y', np.float64),
    ('z', np.float64),
])


def consume_structured(arr: np.ndarray) -> float:
    return float(arr['x'].sum() + arr['y'].sum() + arr['z'].sum())


# ---------------------------------------------------------------------------
# C-Two: transferable + CRM contracts (module-level for spawn pickling)
# ---------------------------------------------------------------------------

# --- pickle variant (separate arrays, full schema with strings) ---

@cc.transferable
class NpPayload:
    row_id: np.ndarray
    x: np.ndarray
    y: np.ndarray
    z: np.ndarray
    name: list

    def serialize(data: 'NpPayload') -> bytes:
        return pickle.dumps(
            (data.row_id, data.x, data.y, data.z, data.name),
            protocol=pickle.HIGHEST_PROTOCOL,
        )

    def deserialize(buf: bytes) -> 'NpPayload':
        row_id, x, y, z, name = pickle.loads(buf)
        return NpPayload(row_id=row_id, x=x, y=y, z=z, name=name)


@cc.crm(namespace='bench.unified.pickle', version='0.1.0')
class INpEcho:
    def echo(self, data: NpPayload) -> NpPayload: ...


class NpEchoCRM:
    def __init__(self, n: int):
        arrays = make_arrays(n)
        self._payload = NpPayload(**arrays)

    def echo(self, data: NpPayload) -> NpPayload:
        return self._payload


# --- hold variant (structured array, zero-copy via from_buffer) ---

@cc.transferable
class NpStructured:
    arr: np.ndarray

    def serialize(data: 'NpStructured') -> bytes:
        return data.arr.tobytes()

    def deserialize(buf: bytes) -> 'NpStructured':
        return NpStructured(arr=np.frombuffer(buf, dtype=COORD_DTYPE).copy())

    def from_buffer(buf: memoryview) -> 'NpStructured':
        return NpStructured(arr=np.frombuffer(buf, dtype=COORD_DTYPE))


@cc.crm(namespace='bench.unified.hold', version='0.1.0')
class INpHoldEcho:
    @cc.transfer(input=NpStructured, output=NpStructured, buffer='hold')
    def echo(self, data: NpStructured) -> NpStructured: ...


class NpHoldEchoCRM:
    def __init__(self, n: int):
        idx = np.arange(n, dtype=np.uint32)
        arr = np.empty(n, dtype=COORD_DTYPE)
        arr['row_id'] = idx
        arr['x'] = idx.astype(np.float64) * 0.1
        arr['y'] = idx.astype(np.float64) * 0.2
        arr['z'] = idx.astype(np.float64) * 0.3
        self._payload = NpStructured(arr=arr)

    def echo(self, data: NpStructured) -> NpStructured:
        return self._payload


# ---------------------------------------------------------------------------
# C-Two benchmark (cross-process IPC via UDS + SHM)
# ---------------------------------------------------------------------------

def _ctwo_server(variant: str, n: int, ready_path: str) -> None:
    cc.set_server(pool_segment_size=2 * 1024 * 1024 * 1024, max_pool_segments=8)
    if variant == 'hold':
        cc.register(INpHoldEcho, NpHoldEchoCRM(n), name='np_echo')
    else:
        cc.register(INpEcho, NpEchoCRM(n), name='np_echo')
    addr = cc.server_address() or ''
    with open(ready_path, 'w') as f:
        f.write(addr)
    cc.serve()


def _ctwo_wait_ready(path: str, timeout: float = 60.0) -> str:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if os.path.exists(path):
            with open(path) as f:
                addr = f.read().strip()
            if addr:
                return addr
        time.sleep(0.05)
    raise TimeoutError('c-two server not ready')


def bench_ctwo(n: int, rounds: int, variant: str = 'pickle') -> dict:
    ready_path = f'/tmp/unified_ctwo_{os.getpid()}_{time.time_ns()}.ready'
    ctx = mp.get_context('spawn')
    proc = ctx.Process(target=_ctwo_server, args=(variant, n, ready_path))
    proc.start()

    try:
        address = _ctwo_wait_ready(ready_path)
        cc.set_client(
            pool_segment_size=2 * 1024 * 1024 * 1024,
            max_pool_segments=8,
        )

        if variant == 'hold':
            proxy = cc.connect(INpHoldEcho, name='np_echo', address=address)
            dummy_arr = np.zeros(1, dtype=COORD_DTYPE)
            dummy = NpStructured(arr=dummy_arr)

            # Warmup
            for _ in range(WARMUP):
                with cc.hold(proxy.echo)(dummy) as held:
                    _ = consume_structured(held.value.arr)

            # Measure
            latencies: list[float] = []
            gc.disable()
            try:
                for _ in range(rounds):
                    t0 = time.perf_counter()
                    with cc.hold(proxy.echo)(dummy) as held:
                        _ = consume_structured(held.value.arr)
                    latencies.append((time.perf_counter() - t0) * 1000)
            finally:
                gc.enable()
        else:
            proxy = cc.connect(INpEcho, name='np_echo', address=address)
            z = np.zeros(1, dtype=np.uint32)
            zf = np.zeros(1, dtype=np.float64)
            dummy = NpPayload(row_id=z, x=zf, y=zf, z=zf, name=['x'])

            # Warmup
            for _ in range(WARMUP):
                result = proxy.echo(dummy)
                _ = consume({
                    'x': result.x, 'y': result.y, 'z': result.z,
                })

            # Measure
            latencies: list[float] = []
            gc.disable()
            try:
                for _ in range(rounds):
                    t0 = time.perf_counter()
                    result = proxy.echo(dummy)
                    _ = consume({
                        'x': result.x, 'y': result.y, 'z': result.z,
                    })
                    latencies.append((time.perf_counter() - t0) * 1000)
            finally:
                gc.enable()

        cc.close(proxy)
        cc.shutdown()

        return {
            'p50_ms': statistics.median(latencies),
            'mean_ms': statistics.fmean(latencies),
            'min_ms': min(latencies),
            'max_ms': max(latencies),
        }
    finally:
        proc.terminate()
        proc.join(timeout=5)
        try:
            os.unlink(ready_path)
        except FileNotFoundError:
            pass


# ---------------------------------------------------------------------------
# Ray: actor (module-level for pickling)
# ---------------------------------------------------------------------------

@ray.remote
class NpSource:
    def __init__(self):
        self._cache: dict[int, dict] = {}

    def echo(self, n: int):
        if n not in self._cache:
            self._cache[n] = make_arrays(n)
        return self._cache[n]


# ---------------------------------------------------------------------------
# Ray benchmark (cross-process via object store)
# ---------------------------------------------------------------------------

def bench_ray(n: int, rounds: int) -> dict:
    actor = NpSource.remote()
    ray.get(actor.echo.remote(10))  # warmup actor

    # Warmup
    for _ in range(WARMUP):
        result = ray.get(actor.echo.remote(n))
        _ = consume(result)

    # Measure
    latencies: list[float] = []
    gc.disable()
    try:
        for _ in range(rounds):
            t0 = time.perf_counter()
            result = ray.get(actor.echo.remote(n))
            _ = consume(result)
            latencies.append((time.perf_counter() - t0) * 1000)
            del result
    finally:
        gc.enable()

    return {
        'p50_ms': statistics.median(latencies),
        'mean_ms': statistics.fmean(latencies),
        'min_ms': min(latencies),
        'max_ms': max(latencies),
    }


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> None:
    parser = argparse.ArgumentParser(
        description='Unified NumPy IPC benchmark: C-Two vs Ray',
    )
    parser.add_argument(
        '--sizes', type=int, nargs='+',
        help='Row counts to test (default: 1K 10K 100K 1M 3M)',
    )
    args = parser.parse_args()

    sizes = DEFAULT_SIZES
    if args.sizes:
        label_map = {1_000: '1K', 10_000: '10K', 100_000: '100K',
                     1_000_000: '1M', 3_000_000: '3M', 5_000_000: '5M',
                     10_000_000: '10M'}
        sizes = [(s, label_map.get(s, f'{s}')) for s in args.sizes]

    # Initialize Ray once for all sizes
    ray.init(
        num_cpus=2,
        object_store_memory=2 * 1024 * 1024 * 1024,
        include_dashboard=False,
        ignore_reinit_error=True,
        logging_level='WARNING',
        _system_config={'automatic_object_spilling_enabled': False},
    )

    print()
    print('=' * 86)
    print('Unified NumPy IPC Benchmark — C-Two vs Ray')
    print(f'Python {sys.version.split()[0]}  |  NumPy {np.__version__}')
    print(f'Ray {ray.__version__}  |  C-Two {cc.__version__}')
    print(f'Payload: row_id(u32) + x,y,z(f64)')
    print(f'Warmup: {WARMUP}  |  Adaptive rounds')
    print('=' * 86)
    print(f'{"N":>8}  {"Rnds":>4}  '
          f'{"C2 pickle":>11}  {"C2 hold":>11}  {"Ray":>11}  '
          f'{"Ray/Hold":>8}')
    print('-' * 86)

    rows: list[dict] = []

    for n, label in sizes:
        rounds = rounds_for(n)
        sys.stdout.write(f'{label:>8}  {rounds:>4}  ')
        sys.stdout.flush()

        # C-Two pickle
        try:
            ctwo_pickle = bench_ctwo(n, rounds, variant='pickle')
            sys.stdout.write(f'{ctwo_pickle["p50_ms"]:>9.2f}ms  ')
        except Exception as e:
            ctwo_pickle = None
            sys.stdout.write(f'{"FAIL":>11}  ')
            print(f'[C-Two pickle error: {e}]', file=sys.stderr)
        sys.stdout.flush()

        # C-Two hold
        try:
            ctwo_hold = bench_ctwo(n, rounds, variant='hold')
            sys.stdout.write(f'{ctwo_hold["p50_ms"]:>9.2f}ms  ')
        except Exception as e:
            ctwo_hold = None
            sys.stdout.write(f'{"FAIL":>11}  ')
            print(f'[C-Two hold error: {e}]', file=sys.stderr)
        sys.stdout.flush()

        # Ray
        try:
            r = bench_ray(n, rounds)
            sys.stdout.write(f'{r["p50_ms"]:>9.2f}ms  ')
        except Exception as e:
            r = None
            sys.stdout.write(f'{"FAIL":>11}  ')
            print(f'[Ray error: {e}]', file=sys.stderr)

        # Ratio (Ray / C-Two hold)
        if ctwo_hold and r:
            ratio = r['p50_ms'] / ctwo_hold['p50_ms']
            sys.stdout.write(f'{ratio:>6.1f}×')
            rows.append({
                'label': label, 'n': n, 'rounds': rounds,
                'ctwo_pickle_p50': ctwo_pickle['p50_ms'] if ctwo_pickle else None,
                'ctwo_hold_p50': ctwo_hold['p50_ms'],
                'ray_p50': r['p50_ms'],
                'ratio': ratio,
            })
        else:
            sys.stdout.write(f'{"—":>8}')

        print()

    ray.shutdown()

    print('=' * 86)
    print('C2 pickle = C-Two IPC with pickle serialization (copy)')
    print('C2 hold   = C-Two IPC with SHM hold mode (zero-copy np.frombuffer)')
    print('Ray       = Ray actor (object store, zero-copy numpy)')
    print('Ray/Hold  = Ray P50 / C-Two hold P50 (>1× means C-Two hold is faster)')

    # Write results
    results_dir = os.path.join(os.path.dirname(__file__), 'results')
    os.makedirs(results_dir, exist_ok=True)
    out_path = os.path.join(results_dir, 'unified_numpy.txt')
    with open(out_path, 'w') as f:
        f.write('# Unified NumPy IPC Benchmark — C-Two vs Ray\n')
        f.write(f'# Python {sys.version.split()[0]}  NumPy {np.__version__}\n')
        f.write(f'# Ray {ray.__version__}  C-Two {cc.__version__}\n')
        f.write(f'{"N":>8}\t{"Rounds":>6}\t{"C2_pickle_ms":>14}\t'
                f'{"C2_hold_ms":>14}\t{"Ray_ms":>14}\t{"Ratio":>8}\n')
        for row in rows:
            pkl = f'{row["ctwo_pickle_p50"]:.4f}' if row['ctwo_pickle_p50'] else 'N/A'
            f.write(f'{row["label"]:>8}\t{row["rounds"]:>6}\t'
                    f'{pkl:>14}\t{row["ctwo_hold_p50"]:>14.4f}\t'
                    f'{row["ray_p50"]:>14.4f}\t{row["ratio"]:>7.1f}×\n')
    print(f'\nResults written to {out_path}')


if __name__ == '__main__':
    main()
