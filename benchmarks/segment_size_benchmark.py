"""Benchmark: IPC performance across segment sizes (256MB vs 2GB).

Compares buddy vs dedicated allocation paths for payloads 64B–1GB.
Tests both bytes (identity fast path) and dict (pickle serialization).

Usage:
    C2_RELAY_ADDRESS= uv run python benchmarks/segment_size_benchmark.py
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
from c_two.transport.registry import _ProcessRegistry

# ── Echo CRMs ─────────────────────────────────────────────────────────

@cc.icrm(namespace='bench.seg_bytes', version='0.1.0')
class IEcho:
    def echo(self, data: bytes) -> bytes: ...

class Echo:
    def echo(self, data: bytes) -> bytes:
        return data


@cc.icrm(namespace='bench.seg_dict', version='0.1.0')
class IDictEcho:
    def echo(self, data: dict) -> dict: ...

class DictEcho:
    def echo(self, data: dict) -> dict:
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

SEG_CONFIGS = [
    (256 * 1024 * 1024,  '256MB-seg'),
    (2 * 1024 * 1024 * 1024, '2GB-seg'),
]

def _rounds(size: int) -> int:
    if size <= 1024 * 1024:       return 100
    if size <= 100 * 1024 * 1024: return 20
    return 5

_counter = 0

def _wait_sock(address: str, timeout: float = 5.0) -> None:
    """Wait for the UDS socket file to appear."""
    import pathlib
    sock = pathlib.Path(f'/tmp/c_two_ipc/{address.removeprefix("ipc://")}.sock')
    t0 = time.monotonic()
    while not sock.exists():
        if time.monotonic() - t0 > timeout:
            break
        time.sleep(0.01)


def _cleanup() -> None:
    for f in glob.glob('/tmp/c_two_ipc/bench_seg_*.sock'):
        try:
            os.unlink(f)
        except OSError:
            pass


# ── Benchmark runners ─────────────────────────────────────────────────

def bench_ipc_bytes(payload_size: int, seg_size: int) -> float | None:
    global _counter
    _counter += 1
    _ProcessRegistry.reset()

    address = f'ipc://bench_seg_{_counter}'
    cc.set_server_ipc_config(segment_size=seg_size, max_segments=8,
                              reassembly_segment_size=seg_size, reassembly_max_segments=8)
    cc.set_client_ipc_config(segment_size=seg_size, max_segments=8,
                              reassembly_segment_size=seg_size, reassembly_max_segments=8)
    cc.set_ipc_address(address)
    cc.register(IEcho, Echo(), name='echo_b')
    _wait_sock(address)

    payload = b'\xAB' * payload_size
    rounds = _rounds(payload_size)
    try:
        icrm = cc.connect(IEcho, name='echo_b', address=address)
        # warmup
        for _ in range(3):
            r = icrm.echo(payload)
            if hasattr(r, 'release'): r.release()
        gc.collect()

        latencies = []
        for _ in range(rounds):
            t0 = time.perf_counter()
            r = icrm.echo(payload)
            t1 = time.perf_counter()
            if hasattr(r, 'release'): r.release()
            latencies.append((t1 - t0) * 1000)

        cc.close(icrm)
        return statistics.median(latencies)
    except Exception as exc:
        print(f'  [bytes FAILED: {exc}]', file=sys.stderr)
        return None
    finally:
        cc.unregister('echo_b')
        cc.shutdown()
        del payload
        gc.collect()


def bench_ipc_dict(payload_size: int, seg_size: int) -> float | None:
    global _counter
    _counter += 1
    _ProcessRegistry.reset()

    address = f'ipc://bench_seg_{_counter}'
    cc.set_server_ipc_config(segment_size=seg_size, max_segments=8,
                              reassembly_segment_size=seg_size, reassembly_max_segments=8)
    cc.set_client_ipc_config(segment_size=seg_size, max_segments=8,
                              reassembly_segment_size=seg_size, reassembly_max_segments=8)
    cc.set_ipc_address(address)
    cc.register(IDictEcho, DictEcho(), name='echo_d')
    _wait_sock(address)

    # Build a dict payload of approximately the target size.
    # Use a large binary field to control size precisely.
    payload = {'data': b'\xCD' * payload_size, 'meta': {'size': payload_size}}
    rounds = _rounds(payload_size)
    try:
        icrm = cc.connect(IDictEcho, name='echo_d', address=address)
        for _ in range(3):
            r = icrm.echo(payload)
            if hasattr(r, 'release'): r.release()
        gc.collect()

        latencies = []
        for _ in range(rounds):
            t0 = time.perf_counter()
            r = icrm.echo(payload)
            t1 = time.perf_counter()
            if hasattr(r, 'release'): r.release()
            latencies.append((t1 - t0) * 1000)

        cc.close(icrm)
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
    _cleanup()

    py_ver = sys.version.split('\n')[0]
    print('=' * 120)
    print('Segment Size Benchmark: 256MB (buddy+dedicated) vs 2GB (all-buddy)')
    print(f'Payload types: bytes (identity) | dict (pickle)   Python: {py_ver}')
    print('=' * 120)

    # Collect all results: results[seg_label][size_label] = (bytes_ms, dict_ms)
    results: dict[str, dict[str, tuple[float | None, float | None]]] = {}

    for seg_size, seg_label in SEG_CONFIGS:
        results[seg_label] = {}
        print(f'\n--- {seg_label} (segment_size = {seg_size / (1024*1024):.0f} MB) ---')
        print(f'{"Size":>8}  {"Rounds":>6}  {"bytes P50":>12}  {"dict P50":>12}  {"dict/bytes":>10}')
        print('-' * 64)

        for size, size_label in SIZES:
            rounds = _rounds(size)
            b_ms = bench_ipc_bytes(size, seg_size)
            d_ms = bench_ipc_dict(size, seg_size)
            results[seg_label][size_label] = (b_ms, d_ms)

            b_str = f'{b_ms:.3f}' if b_ms else 'FAIL'
            d_str = f'{d_ms:.3f}' if d_ms else 'FAIL'
            ratio = f'{d_ms/b_ms:.1f}×' if b_ms and d_ms else 'N/A'
            print(f'{size_label:>8}  {rounds:>6}  {b_str:>12}  {d_str:>12}  {ratio:>10}')
            sys.stdout.flush()

    # Comparison table
    print('\n' + '=' * 120)
    print('Comparison: 256MB-seg vs 2GB-seg (lower is better)')
    print('=' * 120)
    print(f'{"Size":>8}  {"256M-bytes":>12}  {"2G-bytes":>12}  {"speedup":>8}  '
          f'{"256M-dict":>12}  {"2G-dict":>12}  {"speedup":>8}  {"alloc path (256M)":>20}')
    print('-' * 120)

    for size, size_label in SIZES:
        r256 = results.get('256MB-seg', {}).get(size_label, (None, None))
        r2g  = results.get('2GB-seg', {}).get(size_label, (None, None))

        b256 = f'{r256[0]:.3f}' if r256[0] else 'FAIL'
        b2g  = f'{r2g[0]:.3f}' if r2g[0] else 'FAIL'
        bspd = f'{r256[0]/r2g[0]:.2f}×' if r256[0] and r2g[0] else 'N/A'

        d256 = f'{r256[1]:.3f}' if r256[1] else 'FAIL'
        d2g  = f'{r2g[1]:.3f}' if r2g[1] else 'FAIL'
        dspd = f'{r256[1]/r2g[1]:.2f}×' if r256[1] and r2g[1] else 'N/A'

        # Allocation path for 256MB segments
        if size <= 256 * 1024 * 1024:
            path = 'buddy'
        else:
            path = 'DEDICATED'

        print(f'{size_label:>8}  {b256:>12}  {b2g:>12}  {bspd:>8}  '
              f'{d256:>12}  {d2g:>12}  {dspd:>8}  {path:>20}')

    # TSV output for programmatic use
    print('\n--- TSV DATA ---')
    print('size\tseg_config\tbytes_p50_ms\tdict_p50_ms')
    for seg_size, seg_label in SEG_CONFIGS:
        for size, size_label in SIZES:
            r = results.get(seg_label, {}).get(size_label, (None, None))
            b = f'{r[0]:.4f}' if r[0] else ''
            d = f'{r[1]:.4f}' if r[1] else ''
            print(f'{size_label}\t{seg_label}\t{b}\t{d}')

    _cleanup()


if __name__ == '__main__':
    main()
