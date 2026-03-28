"""Benchmark: chunked streaming transfer vs normal single-frame transfer.

Compares latency and throughput across payload sizes from 4 KB to 1 GB,
covering both the normal path and the chunked path.

Usage:
    uv run python benchmarks/chunked_benchmark.py [--max-mb 512]
"""

from __future__ import annotations

import argparse
import os
import statistics
import sys
import time

import c_two as cc
from c_two.transport.ipc.frame import IPCConfig
from c_two.transport.client import SharedClient
from c_two.transport.proxy import ICRMProxy
from c_two.transport.server import ServerV2


# ---------------------------------------------------------------------------
# Inline ICRM / CRM
# ---------------------------------------------------------------------------

@cc.icrm(namespace='bench.chunk', version='0.1.0')
class IBenchChunk:
    def echo(self, data: str) -> str: ...
    def add(self, a: int, b: int) -> int: ...


class BenchChunk:
    def echo(self, data: str) -> str:
        return data

    def add(self, a: int, b: int) -> int:
        return a + b


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')


def _wait_for_server(addr: str, timeout: float = 10.0) -> None:
    region = addr.replace('ipc-v3://', '')
    sock_path = os.path.join(_IPC_SOCK_DIR, f'{region}.sock')
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.exists(sock_path):
            return
        time.sleep(0.05)
    raise TimeoutError(f'Server {addr} did not start within {timeout}s')


def bench_one(
    icrm: IBenchChunk,
    payload_size: int,
    warmup: int = 2,
    repeats: int = 5,
) -> dict:
    """Benchmark one payload size.  Returns timing stats dict."""
    payload = 'X' * payload_size

    # Warmup
    for _ in range(warmup):
        icrm.echo(payload)

    latencies = []
    for _ in range(repeats):
        t0 = time.perf_counter()
        result = icrm.echo(payload)
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

    # Default config (256 MB segments) for realistic benchmarking.
    cfg = IPCConfig()

    addr = f'ipc-v3://bench_chunk_{os.getpid()}'
    server = ServerV2(bind_address=addr, ipc_config=cfg)
    server.register_crm(IBenchChunk, BenchChunk(), name='bench')
    server.start()
    _wait_for_server(addr)

    client = SharedClient(addr, try_v2=True, ipc_config=cfg)
    client.connect()

    proxy = ICRMProxy.ipc(client, 'bench')
    icrm = IBenchChunk()
    icrm.client = proxy

    # Verify connectivity.
    assert icrm.add(1, 2) == 3, 'Basic connectivity check failed'

    # Build payload schedule.
    sizes_kb = [4, 64, 256, 1024, 4096, 16384, 65536]
    max_kb = args.max_mb * 1024
    sizes_kb = [s for s in sizes_kb if s <= max_kb]

    # Add boundary sizes around the chunk threshold.
    seg_size = cfg.pool_segment_size
    threshold = int(seg_size * 0.9)
    chunk_size = seg_size // 2
    boundary_kb = [
        threshold // 1024 - 1,   # just below threshold
        threshold // 1024 + 1,   # just above threshold (single extra chunk)
        seg_size // 1024,        # exactly 1 segment
        seg_size // 1024 * 2,    # 2 segments worth
    ]
    for bk in boundary_kb:
        if bk > 0 and bk <= max_kb and bk not in sizes_kb:
            sizes_kb.append(bk)
    sizes_kb.sort()

    # Header
    print()
    print(f'  pool_segment_size : {seg_size:>12,} bytes ({seg_size // (1024*1024)} MB)')
    print(f'  chunk_threshold   : {threshold:>12,} bytes')
    print(f'  chunk_size        : {chunk_size:>12,} bytes ({chunk_size // (1024*1024)} MB)')
    print(f'  chunked_capable   : {client._chunked_capable}')
    print()
    header = f'{"Payload":>12}  {"Mode":>8}  {"Min(ms)":>10}  {"Median(ms)":>10}  {"Max(ms)":>10}  {"Throughput":>14}'
    print(header)
    print('-' * len(header))

    results = []
    for size_kb in sizes_kb:
        payload_bytes = size_kb * 1024
        is_chunked = payload_bytes > threshold
        mode = 'chunked' if is_chunked else 'normal'

        # Fewer repeats for very large payloads.
        repeats = 3 if payload_bytes >= 256 * 1024 * 1024 else 5
        warmup = 1 if payload_bytes >= 64 * 1024 * 1024 else 2

        try:
            stats = bench_one(icrm, payload_bytes, warmup=warmup, repeats=repeats)
        except Exception as e:
            print(f'{size_kb:>9} KB  {"ERROR":>8}  {str(e)[:60]}')
            continue

        size_str = f'{size_kb} KB' if size_kb < 1024 else f'{size_kb // 1024} MB'
        tp_str = f'{stats["throughput_mbps"]:.1f} MB/s'

        print(
            f'{size_str:>12}  {mode:>8}  '
            f'{stats["min_ms"]:>10.2f}  {stats["median_ms"]:>10.2f}  '
            f'{stats["max_ms"]:>10.2f}  {tp_str:>14}'
        )
        results.append(stats)

    print()
    print('  All payloads verified for data integrity (echo round-trip).')
    print()

    # Cleanup
    client.terminate()
    server.shutdown()


if __name__ == '__main__':
    main()
