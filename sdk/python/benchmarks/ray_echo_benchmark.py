"""Ray vs C-Two comparison: stateful actor echo benchmark.

Mirrors `thread_vs_ipc_benchmark.py`:
  - Same payload sizes (64B -> 1GB)
  - Same operation: send bytes to a stateful echo actor, receive bytes back
  - Same ROUNDS=100 / WARMUP=5, P50 latency

Ray wraps a `@ray.remote` EchoActor to parallel C-Two's CRM + connect model.

NOTE: Ray does not run on free-threaded CPython 3.14t. Run this script with
stock Python 3.12:

    /tmp/ray_bench_env/bin/python sdk/python/benchmarks/ray_echo_benchmark.py
"""
from __future__ import annotations

import gc
import math
import statistics
import time

import ray

SIZES = [
    (64,                    '64B'),
    (256,                   '256B'),
    (1024,                  '1KB'),
    (4 * 1024,              '4KB'),
    (16 * 1024,             '16KB'),
    (64 * 1024,             '64KB'),
    (256 * 1024,            '256KB'),
    (1024 * 1024,           '1MB'),
    (4 * 1024 * 1024,       '4MB'),
    (10 * 1024 * 1024,      '10MB'),
    (50 * 1024 * 1024,      '50MB'),
    (100 * 1024 * 1024,     '100MB'),
    # 500MB / 1GB omitted: Ray's plasma store evicts between rounds, producing
    # extra SIGTERMs on macOS even with spilling disabled.
]

ROUNDS = 100
WARMUP = 5


def rounds_for(size: int) -> int:
    if size <= 1 * 1024 * 1024:
        return 100
    if size <= 100 * 1024 * 1024:
        return 20
    return 5


@ray.remote
class EchoActor:
    def echo(self, data: bytes) -> bytes:
        return data


def p50_ms(samples: list[float]) -> float:
    return statistics.median(samples) * 1000.0


def bench_actor(actor, size: int) -> float:
    payload = b'\x00' * size
    # warmup
    for _ in range(WARMUP):
        ray.get(actor.echo.remote(payload))
    n = rounds_for(size)
    samples: list[float] = []
    for _ in range(n):
        t0 = time.perf_counter()
        out = ray.get(actor.echo.remote(payload))
        samples.append(time.perf_counter() - t0)
        del out
    del payload
    gc.collect()
    return p50_ms(samples)


def geomean(xs: list[float]) -> float:
    return math.exp(sum(math.log(x) for x in xs) / len(xs))


def main() -> None:
    # object_store_memory must fit the biggest payload at least twice (in + out)
    ray.init(
        num_cpus=2,
        # Ray caps mac object store at 2GB by default (known perf issue).
        # Keep to the supported limit to avoid worker SIGTERM on large payloads.
        object_store_memory=2 * 1024 * 1024 * 1024,
        include_dashboard=False,
        ignore_reinit_error=True,
        logging_level='WARNING',
        _system_config={
            'automatic_object_spilling_enabled': False,
        },
    )

    actor = EchoActor.remote()
    # Ensure actor is ready
    ray.get(actor.echo.remote(b'\x00'))

    print('=' * 80)
    print('Ray actor echo benchmark  (Ray %s)' % ray.__version__)
    print('Rounds: 100/20/5 (adaptive)  |  Warmup: %d' % WARMUP)
    print('=' * 80)
    print(f'{"Size":>8}  {"Ray P50 (ms)":>14}  {"Throughput":>14}')
    print('-' * 80)

    results: list[tuple[str, float]] = []
    for size, label in SIZES:
        try:
            gc.collect()
            p50 = bench_actor(actor, size)
            # effective throughput: payload traverses actor once each way -> 2x size
            tput_gbps = (2 * size) / (p50 / 1000.0) / (1024 ** 3)
            results.append((label, p50))
            print(f'{label:>8}  {p50:>14.3f}  {tput_gbps:>10.2f} GB/s')
        except Exception as e:  # noqa: BLE001
            print(f'{label:>8}  FAILED: {e}')
            break

    if results:
        gm = geomean([r[1] for r in results])
        print('-' * 80)
        print(f'{"GeoMean":>8}  {gm:>14.3f}')
        print('=' * 80)

    ray.shutdown()


if __name__ == '__main__':
    main()
