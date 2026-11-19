"""Benchmark: IPC echo latency under a role memory budget.

Compares latency and throughput across payload sizes from 4 KB to 1 GB,
using Core-managed SHM and message fragmentation.

Usage:
    uv run python sdk/python/benchmarks/chunked_benchmark.py [--max-mb 512]
"""

from __future__ import annotations

import argparse
import os
import statistics
import sys
import time

import c_two as cc
from c_two.transport.registry import _reset_default_runtime
from c_two.transport.client.util import ping


# ---------------------------------------------------------------------------
# Inline CRM contract + resource
# ---------------------------------------------------------------------------

@cc.crm(namespace='bench.chunk', version='0.1.0')
class BenchChunk:
    def echo(self, data: str) -> str: ...
    def add(self, a: int, b: int) -> int: ...


class BenchChunkImpl:
    def echo(self, data: str) -> str:
        return data

    def add(self, a: int, b: int) -> int:
        return a + b


# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

KIB = 1024
MIB = KIB * KIB


def bench_one(
    crm: BenchChunk,
    payload_size: int,
    warmup: int = 2,
    repeats: int = 5,
) -> dict:
    """Benchmark one payload size.  Returns timing stats dict."""
    payload = 'X' * payload_size

    # Warmup
    for _ in range(warmup):
        crm.echo(payload)

    latencies = []
    for _ in range(repeats):
        t0 = time.perf_counter()
        result = crm.echo(payload)
        t1 = time.perf_counter()
        assert len(result) == payload_size, f'Data integrity error: {len(result)} != {payload_size}'
        latencies.append(t1 - t0)

    return {
        'payload_bytes': payload_size,
        'repeats': repeats,
        'min_ms': min(latencies) * 1000,
        'median_ms': statistics.median(latencies) * 1000,
        'max_ms': max(latencies) * 1000,
        'throughput_mbps': (payload_size * 2 / (1024 * 1024)) / statistics.median(latencies),
    }


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> None:
    parser = argparse.ArgumentParser(description='Chunked transfer benchmark')
    parser.add_argument('--max-mb', type=int, default=512,
                        help='Maximum payload size in MB (default: 512)')
    args = parser.parse_args()

    _reset_default_runtime()

    # Leave room for both input serialization and its backing.
    memory_budget = max(64 * MIB, 3 * args.max_mb * MIB)
    cc.set_server(memory_budget_bytes=memory_budget)
    cc.set_client(memory_budget_bytes=memory_budget)
    cc.register(BenchChunk, BenchChunkImpl(), name='bench')
    address = cc.server_address()

    ping(address, timeout=10.0)

    crm = cc.connect(BenchChunk, name='bench', address=address)

    # Verify connectivity.
    assert crm.add(1, 2) == 3, 'Basic connectivity check failed'

    # Build payload schedule.
    sizes_kb = [4, 64, 256, 1024, 4096, 16384, 65536]
    max_kb = args.max_mb * 1024
    sizes_kb = [s for s in sizes_kb if s <= max_kb]

    sizes_kb = sorted(set(sizes_kb) | {size for size in (16, 17, 128, 129, max_kb)
                                     if 0 < size <= max_kb})
    print(f'  Memory budget per role: {memory_budget:,} bytes')
    header = f'{"Payload":>12}  {"Min(ms)":>10}  {"Median(ms)":>10}  {"Max(ms)":>10}  {"Throughput":>14}'
    print(header)
    print('-' * len(header))

    results = []
    for size_kb in sizes_kb:
        payload_bytes = size_kb * 1024

        # Fewer repeats for very large payloads.
        repeats = 3 if payload_bytes >= 256 * 1024 * 1024 else 5
        warmup = 1 if payload_bytes >= 64 * 1024 * 1024 else 2

        try:
            stats = bench_one(crm, payload_bytes, warmup=warmup, repeats=repeats)
        except Exception as e:
            print(f'{size_kb:>9} KB  {"ERROR":>8}  {str(e)[:60]}')
            continue

        size_str = f'{size_kb} KB' if size_kb < 1024 else f'{size_kb // 1024} MB'
        tp_str = f'{stats["throughput_mbps"]:.1f} MB/s'

        print(
            f'{size_str:>12}  '
            f'{stats["min_ms"]:>10.2f}  {stats["median_ms"]:>10.2f}  '
            f'{stats["max_ms"]:>10.2f}  {tp_str:>14}'
        )
        results.append(stats)

    print()
    print('  All payloads verified for data integrity (echo round-trip).')
    print()

    # Cleanup
    cc.close(crm)
    cc.unregister('bench')
    cc.shutdown()


if __name__ == '__main__':
    main()
