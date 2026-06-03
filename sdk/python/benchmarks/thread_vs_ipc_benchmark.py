"""Benchmark: SOTA API thread-local vs IPC communication.

Compares cc.connect() in two modes:
  - Thread-local (same process, zero serialization)
  - IPC (full serialize + SHM + UDS, 2GB buddy pool via cc.set_server / cc.set_client)

Uses a small Python dataclass payload; IPC uses Python pickle fallback.
100 rounds per size, P50 latency.

Usage:
    uv run python sdk/python/benchmarks/thread_vs_ipc_benchmark.py
"""
from __future__ import annotations

import gc
import math
import os
import statistics
import time
from dataclasses import dataclass

import c_two as cc
from c_two.transport.client.util import _socket_path_from_address


# ---------------------------------------------------------------------------
# Benchmark CRM
# ---------------------------------------------------------------------------

@dataclass
class Payload:
    data: bytes


@cc.crm(namespace='bench.thread_ipc', version='0.1.0')
class Echo:
    def echo(self, payload: Payload) -> Payload: ...


class EchoImpl:
    def echo(self, payload: Payload) -> Payload:
        return payload


# ---------------------------------------------------------------------------
# Test matrix
# ---------------------------------------------------------------------------

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
    (500 * 1024 * 1024,     '500MB'),
    (1024 * 1024 * 1024,    '1GB'),
]

ROUNDS = 100
WARMUP = 5

# ---------------------------------------------------------------------------
# Thread-local path — cc.connect() without address (zero serde)
# ---------------------------------------------------------------------------

def bench_thread(payload_size: int) -> float:
    cc.register(Echo, EchoImpl(), name='echo_thread')
    try:
        crm = cc.connect(Echo, name='echo_thread')
        payload = Payload(data=b'\xAB' * payload_size)

        for _ in range(WARMUP):
            crm.echo(payload)

        latencies: list[float] = []
        gc.disable()
        try:
            for _ in range(ROUNDS):
                t0 = time.perf_counter()
                result = crm.echo(payload)
                elapsed = time.perf_counter() - t0
                latencies.append(elapsed)
        finally:
            gc.enable()

        assert isinstance(result, Payload)
        assert len(result.data) == payload_size

        cc.close(crm)
    finally:
        cc.unregister('echo_thread')
        cc.shutdown()

    return statistics.median(latencies) * 1000


# ---------------------------------------------------------------------------
# IPC path — cc.connect() with explicit address (full serde + SHM)
# ---------------------------------------------------------------------------

_ipc_counter = 0

def bench_ipc(payload_size: int) -> float:
    global _ipc_counter
    _ipc_counter += 1

    # 2 GB buddy segments to handle up to 1 GB payloads.
    cc.set_server(ipc_overrides={
        'pool_segment_size': 2 * 1024 * 1024 * 1024,
        'max_pool_segments': 8,
    })
    cc.set_client(ipc_overrides={
        'pool_segment_size': 2 * 1024 * 1024 * 1024,
        'max_pool_segments': 8,
    })
    cc.register(Echo, EchoImpl(), name='echo_ipc')
    address = cc.server_address()

    # Wait for server socket.
    sock_path = _socket_path_from_address(address)
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        if os.path.exists(sock_path):
            break
        time.sleep(0.05)

    payload = Payload(data=b'\xAB' * payload_size)
    latencies: list[float] = []

    try:
        crm = cc.connect(Echo, name='echo_ipc', address=address)

        for _ in range(WARMUP):
            crm.echo(payload)

        gc.disable()
        try:
            for _ in range(ROUNDS):
                t0 = time.perf_counter()
                result = crm.echo(payload)
                elapsed = time.perf_counter() - t0
                latencies.append(elapsed)
        finally:
            gc.enable()

        assert isinstance(result, Payload)
        assert len(result.data) == payload_size

        cc.close(crm)
    finally:
        cc.unregister('echo_ipc')
        cc.shutdown()

    return statistics.median(latencies) * 1000


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------

def main():
    print('=' * 80)
    print('SOTA API Benchmark: Thread-local vs IPC')
    print(f'Rounds: {ROUNDS}  |  Warmup: {WARMUP}')
    print('=' * 80)
    print(f'{"Size":>8s}  {"Thread P50 (ms)":>15s}  {"IPC P50 (ms)":>13s}  '
          f'{"Speedup":>8s}')
    print('-' * 80)

    thread_log: list[float] = []
    ipc_log: list[float] = []

    for size_bytes, label in SIZES:
        # Thread
        t_ms = bench_thread(size_bytes)
        thread_log.append(t_ms)

        # IPC
        i_ms = bench_ipc(size_bytes)
        ipc_log.append(i_ms)

        speedup = i_ms / t_ms if t_ms > 0 else float('inf')
        print(f'{label:>8s}  {t_ms:>15.3f}  {i_ms:>13.3f}  {speedup:>7.1f}×')

    # Geometric mean speedup
    speedups = [i / t for t, i in zip(thread_log, ipc_log) if t > 0]
    geo_speedup = math.exp(sum(math.log(s) for s in speedups) / len(speedups))

    print('-' * 80)
    print(f'{"GeoMean":>8s}  {_geomean(thread_log):>15.3f}  '
          f'{_geomean(ipc_log):>13.3f}  {geo_speedup:>7.1f}×')
    print('=' * 80)


def _geomean(values: list[float]) -> float:
    return math.exp(sum(math.log(v) for v in values) / len(values))


if __name__ == '__main__':
    main()
