"""Benchmark: memory:// vs ipc:// (v3) — general @transferable path.

Measures P50 latency, min/max latency, throughput, and ops/sec across
payload sizes from 64B to 1GB. Uses realistic CRM with @transferable
serialization (not echo-optimized).

Usage:
    uv run python benchmarks/memory_vs_ipc_v3_bench.py
"""

import gc
import math
import os
import statistics
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..', 'src'))

import c_two as cc
from c_two.rpc.server import Server, _start, ServerConfig
from c_two.rpc.client import Client


# ---------------------------------------------------------------------------
# Transferable payload (general-purpose, not bytes-optimized)
# ---------------------------------------------------------------------------

@cc.transferable
class Payload:
    raw: bytes

    def serialize(data: 'Payload') -> bytes:
        return data.raw

    def deserialize(raw: bytes) -> 'Payload':
        return Payload(raw=bytes(raw) if isinstance(raw, memoryview) else raw)


@cc.icrm(namespace='cc.bench.memvsipc', version='0.1.0')
class IBenchCRM:
    def echo_payload(self, data: Payload) -> Payload:
        ...


class BenchCRM:
    def echo_payload(self, data: Payload) -> Payload:
        return data


# ---------------------------------------------------------------------------
# Benchmark helpers
# ---------------------------------------------------------------------------

SIZES = [
    ('64B',    64),
    ('1KB',    1024),
    ('4KB',    4096),
    ('64KB',   64 * 1024),
    ('1MB',    1024 * 1024),
    ('10MB',   10 * 1024 * 1024),
    ('50MB',   50 * 1024 * 1024),
    ('100MB',  100 * 1024 * 1024),
    ('500MB',  500 * 1024 * 1024),
    ('1GB',    1024 * 1024 * 1024),
]

ROUNDS = 100
WARMUP = 5


def make_payload(size: int) -> Payload:
    """Create a Payload of exactly `size` bytes."""
    if size <= 4096:
        return Payload(raw=os.urandom(size))
    block = os.urandom(4096)
    repeats = size // 4096
    remainder = size % 4096
    raw = block * repeats + block[:remainder]
    return Payload(raw=raw)


def run_protocol(protocol: str, address: str, sizes: list, rounds: int, warmup: int) -> dict:
    """Run benchmark for a single protocol, return results dict."""
    from c_two.rpc.ipc import IPCConfig

    crm = BenchCRM()

    # Configure IPC with 2GB segment + 2GB max frame for ≥500MB payloads.
    ipc_cfg = IPCConfig(
        pool_segment_size=2 * 1024 * 1024 * 1024,
        max_frame_size=2 * 1024 * 1024 * 1024,
        max_pool_memory=4 * 1024 * 1024 * 1024,
    )

    config = ServerConfig(
        name='BenchServer',
        crm=crm,
        icrm=IBenchCRM,
        bind_address=address,
        ipc_config=ipc_cfg if address.startswith('ipc') else None,
    )
    server = Server(config)
    _start(server._state)

    # Wait for server readiness
    for _ in range(50):
        try:
            if Client.ping(address, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.1)

    client = Client(
        address,
        ipc_config=ipc_cfg if address.startswith('ipc') else None,
    )
    results = {}

    for label, size in sizes:
        actual_rounds = rounds
        if size >= 500 * 1024 * 1024:
            actual_rounds = max(10, rounds // 10)
        elif size >= 100 * 1024 * 1024:
            actual_rounds = max(20, rounds // 5)
        elif size >= 50 * 1024 * 1024:
            actual_rounds = max(30, rounds // 3)

        payload = make_payload(size)
        serialized = Payload.serialize(payload)

        # Warmup
        for _ in range(min(warmup, 3)):
            try:
                client.call('echo_payload', serialized)
            except Exception as e:
                print(f'  Warmup error at {label}: {e}')
                break

        # Benchmark
        latencies = []
        errors = 0
        for _ in range(actual_rounds):
            gc.disable()
            t0 = time.perf_counter()
            try:
                resp = client.call('echo_payload', serialized)
                t1 = time.perf_counter()
                latencies.append((t1 - t0) * 1000)  # ms
            except Exception:
                t1 = time.perf_counter()
                errors += 1
            finally:
                gc.enable()

        if latencies:
            latencies.sort()
            p50 = latencies[len(latencies) // 2]
            results[label] = {
                'size': size,
                'rounds': actual_rounds,
                'p50_ms': p50,
                'min_ms': latencies[0],
                'max_ms': latencies[-1],
                'mean_ms': statistics.mean(latencies),
                'throughput_gbs': (size / (1024**3)) / (p50 / 1000) if p50 > 0 else 0,
                'ops_per_sec': 1000.0 / p50 if p50 > 0 else 0,
                'errors': errors,
            }
        else:
            results[label] = {
                'size': size, 'rounds': actual_rounds,
                'p50_ms': 0, 'min_ms': 0, 'max_ms': 0, 'mean_ms': 0,
                'throughput_gbs': 0, 'ops_per_sec': 0, 'errors': errors,
            }

        del payload, serialized
        gc.collect()

        print(f'  {protocol} {label:>6s}: P50={results[label]["p50_ms"]:.3f}ms  '
              f'min={results[label]["min_ms"]:.3f}ms  max={results[label]["max_ms"]:.3f}ms  '
              f'ops={results[label]["ops_per_sec"]:.1f}/s  '
              f'tput={results[label]["throughput_gbs"]:.2f}GB/s  '
              f'errors={errors}')

    client.terminate()
    try:
        Client.shutdown(address, timeout=2.0)
    except Exception:
        pass
    time.sleep(0.5)

    return results


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    print('=' * 90)
    print('  Memory vs IPC v3 Benchmark — General @transferable Path')
    print(f'  Rounds: {ROUNDS} (reduced for ≥50MB), Warmup: {WARMUP}')
    print('=' * 90)
    print()

    pid = os.getpid()

    # --- Memory protocol ---
    print('[memory://]')
    mem_addr = f'memory://bench_mem_{pid}'
    mem_results = run_protocol('memory', mem_addr, SIZES, ROUNDS, WARMUP)
    print()

    # --- IPC v3 protocol ---
    print('[ipc://] (v3)')
    ipc_addr = f'ipc://bench_ipc_{pid}'
    ipc_results = run_protocol('ipc', ipc_addr, SIZES, ROUNDS, WARMUP)
    print()

    # --- Comparison table ---
    print('=' * 110)
    print('  Comparison: memory:// vs ipc:// (v3)')
    print('=' * 110)
    hdr = (f'{"Size":>8s} | {"mem P50":>10s} | {"ipc P50":>10s} | {"Speedup":>8s} | '
           f'{"mem ops":>10s} | {"ipc ops":>10s} | '
           f'{"mem tput":>10s} | {"ipc tput":>10s} | '
           f'{"ipc min":>10s} | {"ipc max":>10s}')
    print(hdr)
    print('-' * 110)

    mem_p50_large = []
    ipc_p50_large = []

    for label, size in SIZES:
        m = mem_results.get(label, {})
        v = ipc_results.get(label, {})
        mp50 = m.get('p50_ms', 0)
        vp50 = v.get('p50_ms', 0)
        speedup = mp50 / vp50 if vp50 > 0 else 0

        print(f'{label:>8s} | {mp50:>9.3f}ms | {vp50:>9.3f}ms | {speedup:>7.2f}x | '
              f'{m.get("ops_per_sec", 0):>9.1f}/s | {v.get("ops_per_sec", 0):>9.1f}/s | '
              f'{m.get("throughput_gbs", 0):>8.2f}GB/s | {v.get("throughput_gbs", 0):>8.2f}GB/s | '
              f'{v.get("min_ms", 0):>9.3f}ms | {v.get("max_ms", 0):>9.3f}ms')

        if size >= 10 * 1024 * 1024:
            if mp50 > 0:
                mem_p50_large.append(mp50)
            if vp50 > 0:
                ipc_p50_large.append(vp50)

    print()
    if mem_p50_large and ipc_p50_large:
        mem_geo = math.exp(sum(math.log(x) for x in mem_p50_large) / len(mem_p50_large))
        ipc_geo = math.exp(sum(math.log(x) for x in ipc_p50_large) / len(ipc_p50_large))
        print(f'  Geomean P50 (≥10MB): memory={mem_geo:.3f}ms  ipc-v3={ipc_geo:.3f}ms  '
              f'speedup={mem_geo/ipc_geo:.1f}x')
    print()


if __name__ == '__main__':
    main()
