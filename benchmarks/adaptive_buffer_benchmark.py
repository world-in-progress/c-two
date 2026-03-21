"""
Adaptive Buffer Benchmark
==========================
Demonstrates the memory behaviour of AdaptiveBuffer vs the old grow-only
bytearray strategy under a realistic variable-payload ICRM workload.

Usage:
    uv run python benchmarks/adaptive_buffer_benchmark.py
"""

import gc
import os
import resource
import sys
import time

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src')))

from c_two.rpc.util.adaptive_buffer import AdaptiveBuffer, AdaptiveBufferConfig


def _rss_mb() -> float:
    """Resident set size in MB (macOS / Linux)."""
    return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / (1 << 20)


def _fmt(n: int) -> str:
    if n >= 1 << 20:
        return f'{n / (1 << 20):.0f} MB'
    if n >= 1 << 10:
        return f'{n / (1 << 10):.0f} KB'
    return f'{n} B'


# ---------------------------------------------------------------------------
# Workload: simulates a GIS CRM that alternates between large grid queries
# and small metadata queries.
# ---------------------------------------------------------------------------

WORKLOAD: list[tuple[str, int, int]] = [
    # (label, payload_size, repeat_count)
    ('Large grid query',    64 << 20,   3),    # 64 MB × 3
    ('Small metadata',      1024,       50),   # 1 KB × 50
    ('Medium tile fetch',   4 << 20,    5),    # 4 MB × 5
    ('Tiny ping/status',    64,         100),  # 64 B × 100
    ('Large grid query',    128 << 20,  2),    # 128 MB × 2
    ('Small metadata',      512,        80),   # 512 B × 80
    ('Idle period (2s)',    0,          1),     # simulate idle
    ('Small metadata',      256,        30),   # 256 B × 30
]


def bench_adaptive() -> list[tuple[str, int]]:
    """Run workload with AdaptiveBuffer; return (label, capacity) trace."""
    cfg = AdaptiveBufferConfig(
        min_size=1 << 20,
        shrink_after_n=3,
        shrink_threshold=0.25,
        decay_after_seconds=1.0,  # short for demo
    )
    ab = AdaptiveBuffer(config=cfg)
    trace: list[tuple[str, int]] = []

    for label, size, repeat in WORKLOAD:
        if size == 0:
            # Simulate idle
            time.sleep(2.0)
            ab.maybe_decay()
            trace.append((label, ab.capacity))
            continue
        for _ in range(repeat):
            buf_src = bytearray(size)  # simulate payload
            dst = ab.acquire(size)
            # In real code, ctypes.memmove copies SHM→buffer here
            dst[:size] = buf_src
        trace.append((f'{label} ×{repeat}', ab.capacity))

    return trace


def bench_grow_only() -> list[tuple[str, int]]:
    """Run same workload with old grow-only strategy."""
    buf: bytearray | None = None
    trace: list[tuple[str, int]] = []

    for label, size, repeat in WORKLOAD:
        if size == 0:
            time.sleep(0.01)  # no decay in old strategy
            trace.append((label, len(buf) if buf else 0))
            continue
        for _ in range(repeat):
            if buf is None or len(buf) < size:
                buf = bytearray(size)
            payload = bytearray(size)
            buf[:size] = payload
        trace.append((f'{label} ×{repeat}', len(buf) if buf else 0))

    return trace


def main() -> None:
    print('=' * 70)
    print('Adaptive Buffer Benchmark — Variable Payload Workload')
    print('=' * 70)
    print()

    # --- Old strategy ---
    gc.collect()
    print('▸ Grow-only (old strategy)')
    print(f'  {"Phase":<35} {"Buffer capacity":>15}')
    print(f'  {"-" * 35} {"-" * 15}')
    old_trace = bench_grow_only()
    for label, cap in old_trace:
        print(f'  {label:<35} {_fmt(cap):>15}')
    old_peak = max(cap for _, cap in old_trace)
    old_final = old_trace[-1][1]
    print()

    # --- Adaptive strategy ---
    gc.collect()
    print('▸ AdaptiveBuffer (new strategy)')
    print(f'  {"Phase":<35} {"Buffer capacity":>15}')
    print(f'  {"-" * 35} {"-" * 15}')
    adaptive_trace = bench_adaptive()
    for label, cap in adaptive_trace:
        print(f'  {label:<35} {_fmt(cap):>15}')
    adaptive_peak = max(cap for _, cap in adaptive_trace)
    adaptive_final = adaptive_trace[-1][1]
    print()

    # --- Summary ---
    print('─' * 70)
    print(f'  {"":35} {"Grow-only":>15} {"Adaptive":>15}')
    print(f'  {"Peak buffer":35} {_fmt(old_peak):>15} {_fmt(adaptive_peak):>15}')
    print(f'  {"Final buffer":35} {_fmt(old_final):>15} {_fmt(adaptive_final):>15}')
    if old_final > 0:
        savings = (1 - adaptive_final / old_final) * 100
        print(f'  {"Memory savings (final)":35} {"":>15} {savings:>14.1f}%')
    print('─' * 70)


if __name__ == '__main__':
    main()
