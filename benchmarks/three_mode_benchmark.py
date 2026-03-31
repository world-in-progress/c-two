"""Benchmark: Thread-local vs IPC vs Relay (HTTP) — payload 64B to 1GB.

Measures P50 round-trip latency for echo(bytes)->bytes across three transport modes:
  - Thread-local: same process, zero serialization
  - IPC: full serialize + SHM + UDS (2GB buddy segments)
  - Relay: HTTP → NativeRelay → IPC → CRM → reverse

Payload sizes: 64B, 256B, 1KB, 4KB, 64KB, 1MB, 10MB, 50MB, 100MB, 500MB, 1GB

Usage:
    C2_RELAY_ADDRESS= uv run python benchmarks/three_mode_benchmark.py
"""
from __future__ import annotations

import gc
import glob
import math
import os
import statistics
import sys
import time

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))

import c_two as cc
from c_two._native import NativeRelay
from c_two.transport.registry import _ProcessRegistry

# ---------------------------------------------------------------------------
# Echo CRM — raw bytes to measure pure transport overhead
# ---------------------------------------------------------------------------

@cc.icrm(namespace='bench.three_mode', version='0.1.0')
class IEcho:
    def echo(self, data: bytes) -> bytes: ...

class Echo:
    def echo(self, data: bytes) -> bytes:
        return data


# ---------------------------------------------------------------------------
# Test matrix
# ---------------------------------------------------------------------------

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

# Adaptive rounds: fewer for larger payloads
def _rounds(size: int) -> int:
    if size <= 1024 * 1024:       # ≤ 1MB
        return 100
    if size <= 100 * 1024 * 1024:  # ≤ 100MB
        return 20
    return 5                       # 500MB, 1GB

WARMUP = 3
_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')
_ipc_counter = 0
_relay_port = 19960 + (os.getpid() % 100)


def _cleanup():
    """Clean up stale IPC sockets and SHM."""
    for f in glob.glob('/tmp/c_two_ipc/bench_3m_*.sock'):
        try:
            os.unlink(f)
        except OSError:
            pass
    try:
        from c_two.mem import cleanup_stale_shm
        cleanup_stale_shm()
    except Exception:
        pass


def _wait_sock(address: str, timeout: float = 5.0):
    region_id = address.split('://')[-1]
    sock_path = os.path.join(_IPC_SOCK_DIR, f'{region_id}.sock')
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.exists(sock_path):
            return True
        time.sleep(0.05)
    return False


def _measure(proxy, payload: bytes, rounds: int) -> float:
    """Warmup + timed rounds, return P50 latency in ms."""
    for _ in range(WARMUP):
        proxy.echo(payload)

    latencies: list[float] = []
    gc.disable()
    try:
        for _ in range(rounds):
            t0 = time.perf_counter()
            result = proxy.echo(payload)
            elapsed = time.perf_counter() - t0
            latencies.append(elapsed)
    finally:
        gc.enable()

    assert len(result) == len(payload), f'size mismatch: {len(result)} != {len(payload)}'
    return statistics.median(latencies) * 1000


# ---------------------------------------------------------------------------
# Thread-local mode
# ---------------------------------------------------------------------------

def bench_thread(payload_size: int) -> float:
    _ProcessRegistry.reset()
    cc.register(IEcho, Echo(), name='echo_thread')
    try:
        icrm = cc.connect(IEcho, name='echo_thread')
        payload = b'\xAB' * payload_size
        result_ms = _measure(icrm, payload, _rounds(payload_size))
        cc.close(icrm)
    finally:
        cc.unregister('echo_thread')
        cc.shutdown()
    return result_ms


# ---------------------------------------------------------------------------
# IPC mode
# ---------------------------------------------------------------------------

def bench_ipc(payload_size: int) -> float:
    global _ipc_counter
    _ipc_counter += 1
    _ProcessRegistry.reset()
    address = f'ipc://bench_3m_ipc_{_ipc_counter}'

    cc.set_ipc_config(segment_size=2 * 1024 * 1024 * 1024, max_segments=8)
    cc.set_address(address)
    cc.register(IEcho, Echo(), name='echo_ipc')
    _wait_sock(address)

    payload = b'\xAB' * payload_size
    try:
        icrm = cc.connect(IEcho, name='echo_ipc', address=address)
        result_ms = _measure(icrm, payload, _rounds(payload_size))
        cc.close(icrm)
    finally:
        cc.unregister('echo_ipc')
        cc.shutdown()
    return result_ms


# ---------------------------------------------------------------------------
# Relay (HTTP) mode
# ---------------------------------------------------------------------------

def bench_relay(payload_size: int) -> float | None:
    global _ipc_counter, _relay_port
    _ipc_counter += 1
    _relay_port += 1
    _ProcessRegistry.reset()
    address = f'ipc://bench_3m_relay_{_ipc_counter}'
    relay_addr = f'127.0.0.1:{_relay_port}'

    cc.set_ipc_config(segment_size=2 * 1024 * 1024 * 1024, max_segments=8)
    cc.set_address(address)
    cc.register(IEcho, Echo(), name='echo_relay')
    _wait_sock(address)

    relay = NativeRelay(relay_addr)
    relay.start()
    relay.register_upstream('echo_relay', address)
    time.sleep(0.3)

    payload = b'\xAB' * payload_size
    try:
        icrm = cc.connect(IEcho, name='echo_relay', address=f'http://{relay_addr}')
        result_ms = _measure(icrm, payload, _rounds(payload_size))
        cc.close(icrm)
        return result_ms
    except Exception as exc:
        print(f'  [relay FAILED: {exc}]', file=sys.stderr)
        return None
    finally:
        try:
            relay.stop()
        except Exception:
            pass
        cc.unregister('echo_relay')
        cc.shutdown()


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------

def _geomean(values: list[float]) -> float:
    valid = [v for v in values if v is not None and v > 0]
    if not valid:
        return 0.0
    return math.exp(sum(math.log(v) for v in valid) / len(valid))


def _fmt(v: float | None) -> str:
    if v is None:
        return '—'
    if v < 1.0:
        return f'{v:.4f}'
    if v < 100:
        return f'{v:.3f}'
    return f'{v:.1f}'


def main():
    _cleanup()

    print('=' * 100)
    print('Three-Mode Benchmark: Thread-local vs IPC vs Relay (HTTP)')
    print(f'Warmup: {WARMUP}  |  Adaptive rounds (100/20/5)')
    print(f'Python: {sys.version}')
    print('=' * 100)
    header = f'{"Size":>8s}  {"Rounds":>6s}  {"Thread (ms)":>12s}  {"IPC (ms)":>12s}  {"Relay (ms)":>12s}  {"IPC/Thd":>8s}  {"Relay/Thd":>10s}'
    print(header)
    print('-' * 100)

    results: list[dict] = []

    for size_bytes, label in SIZES:
        rounds = _rounds(size_bytes)

        t_ms = bench_thread(size_bytes)
        i_ms = bench_ipc(size_bytes)
        r_ms = bench_relay(size_bytes)

        ipc_ratio = f'{i_ms / t_ms:.1f}×' if t_ms > 0 else '—'
        relay_ratio = f'{r_ms / t_ms:.1f}×' if (r_ms is not None and t_ms > 0) else '—'

        print(f'{label:>8s}  {rounds:>6d}  {_fmt(t_ms):>12s}  {_fmt(i_ms):>12s}  {_fmt(r_ms):>12s}  {ipc_ratio:>8s}  {relay_ratio:>10s}')

        results.append({
            'size': label, 'size_bytes': size_bytes, 'rounds': rounds,
            'thread_ms': t_ms, 'ipc_ms': i_ms, 'relay_ms': r_ms,
        })

    # Summary
    t_vals = [r['thread_ms'] for r in results]
    i_vals = [r['ipc_ms'] for r in results]
    r_vals = [r['relay_ms'] for r in results if r['relay_ms'] is not None]

    print('-' * 100)
    print(f'{"GeoMean":>8s}  {"":>6s}  {_fmt(_geomean(t_vals)):>12s}  {_fmt(_geomean(i_vals)):>12s}  {_fmt(_geomean(r_vals)):>12s}')
    print('=' * 100)

    # Write TSV
    tsv_path = os.path.join(os.path.dirname(__file__), '..', 'benchmark_results.tsv')
    with open(tsv_path, 'w') as f:
        f.write('size\tsize_bytes\trounds\tthread_ms\tipc_ms\trelay_ms\n')
        for r in results:
            relay_str = f'{r["relay_ms"]:.4f}' if r['relay_ms'] is not None else 'N/A'
            f.write(f'{r["size"]}\t{r["size_bytes"]}\t{r["rounds"]}\t{r["thread_ms"]:.4f}\t{r["ipc_ms"]:.4f}\t{relay_str}\n')
    print(f'\nResults written to {os.path.abspath(tsv_path)}')

    _cleanup()


if __name__ == '__main__':
    main()
