"""Benchmark: method-name pre-encoding in wire.py

Compares encode_call / write_call_into / decode performance
with and without pre-registered method names.

Usage:
    uv run python benchmarks/wire_preencoding_benchmark.py
"""

import struct
import timeit
import statistics
import sys
import os

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..', 'src'))

from c_two.rpc.event.msg_type import MsgType
from c_two.rpc.util import wire

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

METHOD = 'subdivide_grids'

PAYLOADS = {
    'no payload':     None,
    '64 B':           os.urandom(64),
    '1 KB':           os.urandom(1024),
    '1 MB':           os.urandom(1024 * 1024),
}

ITERATIONS = 200_000     # per scenario for encode/write
REPEAT = 5               # timeit repeats; we take median


def ns_per_call(times: list[float], iters: int) -> float:
    return statistics.median(times) / iters * 1e9


# ---------------------------------------------------------------------------
# Build a manual "no-cache" version for fair A/B comparison
# ---------------------------------------------------------------------------

def encode_call_nocache(method_name: str,
                        payload: bytes | memoryview | None = None) -> bytearray:
    """Original encode_call without cache (baseline)."""
    method_bytes = method_name.encode('utf-8')
    method_len = len(method_bytes)
    payload_len = len(payload) if payload else 0
    total = wire.CALL_HEADER_FIXED + method_len + payload_len
    buf = bytearray(total)
    buf[0] = MsgType.CRM_CALL
    struct.pack_into('<H', buf, 1, method_len)
    buf[3:3 + method_len] = method_bytes
    if payload_len > 0:
        buf[3 + method_len:] = payload
    return buf


def write_call_into_nocache(buf, offset: int, method_name: str,
                            payload: bytes | memoryview | None = None) -> int:
    """Original write_call_into without cache (baseline)."""
    method_bytes = method_name.encode('utf-8')
    method_len = len(method_bytes)
    payload_len = len(payload) if payload else 0
    buf[offset] = MsgType.CRM_CALL
    struct.pack_into('<H', buf, offset + 1, method_len)
    buf[offset + 3:offset + 3 + method_len] = method_bytes
    if payload_len > 0:
        buf[offset + 3 + method_len:offset + 3 + method_len + payload_len] = payload
    return wire.CALL_HEADER_FIXED + method_len + payload_len


# ---------------------------------------------------------------------------
# Benchmark runner
# ---------------------------------------------------------------------------

def run():
    # Ensure method is pre-registered
    wire.preregister_method(METHOD)

    sep = '-' * 72
    print(f'\n{"Wire Pre-Encoding Benchmark":^72}')
    print(f'method = {METHOD!r}  |  repeat = {REPEAT}  |  Python {sys.version.split()[0]}')
    print(sep)

    # ---- encode_call ----
    print(f'\n{"encode_call()":^72}')
    print(f'{"Payload":<14} {"Baseline (ns)":>14} {"Cached (ns)":>14} {"Speedup":>10}')
    print(sep)
    for label, payload in PAYLOADS.items():
        iters = ITERATIONS if payload is None or len(payload) <= 1024 else 5_000
        t_base = timeit.repeat(
            lambda p=payload: encode_call_nocache(METHOD, p),
            number=iters, repeat=REPEAT)
        t_cached = timeit.repeat(
            lambda p=payload: wire.encode_call(METHOD, p),
            number=iters, repeat=REPEAT)
        ns_base = ns_per_call(t_base, iters)
        ns_cached = ns_per_call(t_cached, iters)
        speedup = ns_base / ns_cached if ns_cached > 0 else float('inf')
        print(f'{label:<14} {ns_base:>14.1f} {ns_cached:>14.1f} {speedup:>9.2f}x')

    # ---- write_call_into ----
    print(f'\n{"write_call_into()":^72}')
    print(f'{"Payload":<14} {"Baseline (ns)":>14} {"Cached (ns)":>14} {"Speedup":>10}')
    print(sep)
    for label, payload in PAYLOADS.items():
        iters = ITERATIONS if payload is None or len(payload) <= 1024 else 5_000
        size = wire.CALL_HEADER_FIXED + len(METHOD.encode()) + (len(payload) if payload else 0)
        arena = bytearray(size + 64)

        t_base = timeit.repeat(
            lambda a=arena, p=payload: write_call_into_nocache(a, 0, METHOD, p),
            number=iters, repeat=REPEAT)
        t_cached = timeit.repeat(
            lambda a=arena, p=payload: wire.write_call_into(a, 0, METHOD, p),
            number=iters, repeat=REPEAT)
        ns_base = ns_per_call(t_base, iters)
        ns_cached = ns_per_call(t_cached, iters)
        speedup = ns_base / ns_cached if ns_cached > 0 else float('inf')
        print(f'{label:<14} {ns_base:>14.1f} {ns_cached:>14.1f} {speedup:>9.2f}x')

    # ---- correctness check ----
    print(f'\n{sep}')
    for label, payload in PAYLOADS.items():
        a = bytes(encode_call_nocache(METHOD, payload))
        b = bytes(wire.encode_call(METHOD, payload))
        assert a == b, f'Mismatch for {label}'
    print('✓ Correctness verified: cached output == baseline output for all payloads')
    print()


if __name__ == '__main__':
    run()
