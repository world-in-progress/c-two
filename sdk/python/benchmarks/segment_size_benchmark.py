"""Benchmark: IPC performance across role memory budgets (4 GiB and 16 GiB).

Measures latency for payloads from 64 bytes to 1 GiB.
Tests both bytes (identity fast path) and dict (pickle serialization).

Usage:
    C2_RELAY_ANCHOR_ADDRESS= uv run python sdk/python/benchmarks/segment_size_benchmark.py
"""
from __future__ import annotations

import gc
import math
import statistics
import sys
import time

import c_two as cc
from c_two.transport.client.util import ping
from c_two.transport.registry import _reset_default_runtime

# ── Echo CRMs ─────────────────────────────────────────────────────────

@cc.crm(namespace='bench.seg_bytes', version='0.1.0')
class Echo:
    def echo(self, data: bytes) -> bytes: ...

class EchoImpl:
    def echo(self, data: bytes) -> bytes:
        return data


@cc.crm(namespace='bench.seg_dict', version='0.1.0')
class DictEcho:
    def echo(self, data: dict[str, bytes | dict[str, int]]) -> dict[str, bytes | dict[str, int]]: ...

class DictEchoImpl:
    def echo(self, data: dict[str, bytes | dict[str, int]]) -> dict[str, bytes | dict[str, int]]:
        return data


# ── Config ────────────────────────────────────────────────────────────

SIZES = [
    (64,                    '64B'),
    (256,                   '256B'),
    (1024,                  '1KB'),
    (4 * 1024,              '4KB'),
    (64 * 1024,             '64KB'),
    (1024 * 1024,           '1MB'),
    (10 * 1024 * 1024,      '10MB'),
    (50 * 1024 * 1024,      '50MB'),
    (100 * 1024 * 1024,     '100MB'),
    (500 * 1024 * 1024,     '500MB'),
    (1024 * 1024 * 1024,    '1GB'),
]

BUDGETS = [
    (4 * 1024**3, '4GiB'),
    (16 * 1024**3, '16GiB'),
]

def _rounds(size: int) -> int:
    if size <= 1024 * 1024:       return 100
    if size <= 100 * 1024 * 1024: return 20
    return 5

_counter = 0

def _wait_for_ipc(address: str, timeout: float = 5.0) -> None:
    """Probe the native IPC server until it is responsive."""
    t0 = time.monotonic()
    while not ping(address, timeout=0.5):
        if time.monotonic() - t0 > timeout:
            break
        time.sleep(0.01)


# ── Benchmark runners ─────────────────────────────────────────────────

def bench_ipc_bytes(payload_size: int, memory_budget: int) -> float | None:
    global _counter
    _counter += 1
    _reset_default_runtime()

    cc.set_server(memory_budget_bytes=memory_budget)
    cc.set_client(memory_budget_bytes=memory_budget)
    cc.register(Echo, EchoImpl(), name='echo_b')
    address = cc.server_address()
    _wait_for_ipc(address)

    payload = b'\xAB' * payload_size
    rounds = _rounds(payload_size)
    try:
        crm = cc.connect(Echo, name='echo_b', address=address)
        # warmup
        for _ in range(3):
            r = crm.echo(payload)
        gc.collect()

        latencies = []
        for _ in range(rounds):
            t0 = time.perf_counter()
            r = crm.echo(payload)
            t1 = time.perf_counter()
            latencies.append((t1 - t0) * 1000)

        cc.close(crm)
        return statistics.median(latencies)
    except Exception as exc:
        print(f'  [bytes FAILED: {exc}]', file=sys.stderr)
        return None
    finally:
        cc.unregister('echo_b')
        cc.shutdown()
        del payload
        gc.collect()


def bench_ipc_dict(payload_size: int, memory_budget: int) -> float | None:
    global _counter
    _counter += 1
    _reset_default_runtime()

    cc.set_server(memory_budget_bytes=memory_budget)
    cc.set_client(memory_budget_bytes=memory_budget)
    cc.register(DictEcho, DictEchoImpl(), name='echo_d')
    address = cc.server_address()
    _wait_for_ipc(address)

    # Build a dict payload of approximately the target size.
    # Use a large binary field to control size precisely.
    payload = {'data': b'\xCD' * payload_size, 'meta': {'size': payload_size}}
    rounds = _rounds(payload_size)
    try:
        crm = cc.connect(DictEcho, name='echo_d', address=address)
        for _ in range(3):
            r = crm.echo(payload)
        gc.collect()

        latencies = []
        for _ in range(rounds):
            t0 = time.perf_counter()
            r = crm.echo(payload)
            t1 = time.perf_counter()
            latencies.append((t1 - t0) * 1000)

        cc.close(crm)
        return statistics.median(latencies)
    except Exception as exc:
        print(f'  [dict FAILED: {exc}]', file=sys.stderr)
        return None
    finally:
        cc.unregister('echo_d')
        cc.shutdown()
        del payload
        gc.collect()


# ── Main ──────────────────────────────────────────────────────────────

def main() -> None:

    py_ver = sys.version.split('\n')[0]
    print('=' * 120)
    print('Memory Budget Benchmark: 4 GiB vs 16 GiB')
    print(f'Payload types: bytes (identity) | dict (pickle)   Python: {py_ver}')
    print('=' * 120)

    # Collect all results: results[budget_label][size_label] = (bytes_ms, dict_ms)
    results: dict[str, dict[str, tuple[float | None, float | None]]] = {}

    for memory_budget, budget_label in BUDGETS:
        results[budget_label] = {}
        print(f'\n--- {budget_label} per role ---')
        print(f'{"Size":>8}  {"Rounds":>6}  {"bytes P50":>12}  {"dict P50":>12}  {"dict/bytes":>10}')
        print('-' * 64)

        for size, size_label in SIZES:
            rounds = _rounds(size)
            b_ms = bench_ipc_bytes(size, memory_budget)
            d_ms = bench_ipc_dict(size, memory_budget)
            results[budget_label][size_label] = (b_ms, d_ms)

            b_str = f'{b_ms:.3f}' if b_ms else 'FAIL'
            d_str = f'{d_ms:.3f}' if d_ms else 'FAIL'
            ratio = f'{d_ms/b_ms:.1f}×' if b_ms and d_ms else 'N/A'
            print(f'{size_label:>8}  {rounds:>6}  {b_str:>12}  {d_str:>12}  {ratio:>10}')
            sys.stdout.flush()

    # Comparison table
    print('\n' + '=' * 120)
    print('Comparison: 4GiB vs 16GiB (lower is better)')
    print('=' * 120)
    print(f'{"Size":>8}  {"4G-bytes":>12}  {"16G-bytes":>12}  {"speedup":>8}  '
          f'{"4G-dict":>12}  {"16G-dict":>12}  {"speedup":>8}')
    print('-' * 120)

    for size, size_label in SIZES:
        r256 = results.get('4GiB', {}).get(size_label, (None, None))
        r2g  = results.get('16GiB', {}).get(size_label, (None, None))

        b256 = f'{r256[0]:.3f}' if r256[0] else 'FAIL'
        b2g  = f'{r2g[0]:.3f}' if r2g[0] else 'FAIL'
        bspd = f'{r256[0]/r2g[0]:.2f}×' if r256[0] and r2g[0] else 'N/A'

        d256 = f'{r256[1]:.3f}' if r256[1] else 'FAIL'
        d2g  = f'{r2g[1]:.3f}' if r2g[1] else 'FAIL'
        dspd = f'{r256[1]/r2g[1]:.2f}×' if r256[1] and r2g[1] else 'N/A'

        print(f'{size_label:>8}  {b256:>12}  {b2g:>12}  {bspd:>8}  '
              f'{d256:>12}  {d2g:>12}  {dspd:>8}')

    # TSV output for programmatic use
    print('\n--- TSV DATA ---')
    print('size\tmemory_budget\tbytes_p50_ms\tdict_p50_ms')
    for memory_budget, budget_label in BUDGETS:
        for size, size_label in SIZES:
            r = results.get(budget_label, {}).get(size_label, (None, None))
            b = f'{r[0]:.4f}' if r[0] else ''
            d = f'{r[1]:.4f}' if r[1] else ''
            print(f'{size_label}\t{budget_label}\t{b}\t{d}')



if __name__ == '__main__':
    main()
