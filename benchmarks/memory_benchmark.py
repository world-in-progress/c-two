"""
Memory RPC Benchmark
====================
Measures latency and throughput of the memory:// and thread:// protocols
across a wide range of payload sizes — from 64 bytes to multi-GB — to
expose both fixed overhead and data-proportional costs.

Presets:
    --preset quick   : 64B → 1MB, fast sanity check (~30s)
    --preset standard: 64B → 100MB, balanced coverage (~3min)
    --preset full    : 64B → 1GB, thorough analysis (~15min)
    --preset stress  : 64B → 4GB, extreme stress test (~30min+)

Usage:
    uv run python benchmarks/memory_benchmark.py
    uv run python benchmarks/memory_benchmark.py --preset full
    uv run python benchmarks/memory_benchmark.py --payloads 64B,1MB,512MB,2GB --rounds-override 3
    uv run python benchmarks/memory_benchmark.py --protocols memory --payloads 1GB --rounds-override 2
"""

import argparse
import json
import os
import resource
import statistics
import sys
import threading
import time

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src')))

import c_two as cc
from c_two.rpc.server import _start


# ---------------------------------------------------------------------------
# Fixtures — lightweight ICRM + CRM that echo raw bytes
# ---------------------------------------------------------------------------

@cc.transferable
class BenchPayload:
    data: bytes = b''

    @staticmethod
    def serialize(data: bytes) -> bytes:
        return data

    @staticmethod
    def deserialize(raw: memoryview) -> bytes:
        return bytes(raw)


@cc.icrm(namespace='bench', version='0.1.0')
class IBench:
    def echo(self, data: bytes) -> bytes:
        """Send bytes, receive the same bytes back."""
        ...


class Bench:
    def echo(self, data: bytes) -> bytes:
        return data


# ---------------------------------------------------------------------------
# Size parsing / formatting
# ---------------------------------------------------------------------------

_SIZE_UNITS = {
    'B': 1, 'KB': 1024, 'MB': 1024**2, 'GB': 1024**3, 'TB': 1024**4,
    'K': 1024, 'M': 1024**2, 'G': 1024**3, 'T': 1024**4,
}


def parse_size(s: str) -> int:
    """Parse human-readable size string like '64B', '10MB', '1.5GB' → int bytes."""
    s = s.strip().upper()
    for suffix in sorted(_SIZE_UNITS, key=len, reverse=True):
        if s.endswith(suffix):
            num = s[:-len(suffix)].strip()
            return int(float(num) * _SIZE_UNITS[suffix])
    return int(s)


def human_size(n: int) -> str:
    if n < 1024:
        return f'{n}B'
    elif n < 1024**2:
        return f'{n / 1024:.4g}KB'
    elif n < 1024**3:
        return f'{n / 1024**2:.4g}MB'
    else:
        return f'{n / 1024**3:.4g}GB'


# ---------------------------------------------------------------------------
# Payload presets — auto-scaling rounds based on size
# ---------------------------------------------------------------------------

PRESETS = {
    'quick': [64, 1024, 64 * 1024, 1024**2],
    'standard': [64, 1024, 64 * 1024, 1024**2, 10 * 1024**2, 100 * 1024**2],
    'full': [64, 1024, 64 * 1024, 1024**2, 10 * 1024**2, 100 * 1024**2, 512 * 1024**2, 1024**3],
    'stress': [64, 1024, 64 * 1024, 1024**2, 10 * 1024**2, 100 * 1024**2, 512 * 1024**2, 1024**3, 2 * 1024**3, 4 * 1024**3],
}


def auto_rounds(payload_size: int, base_rounds: int = 100) -> int:
    """Scale rounds inversely with payload size to keep test duration reasonable."""
    if payload_size <= 1024**2:         # ≤ 1MB: full rounds
        return base_rounds
    elif payload_size <= 10 * 1024**2:  # ≤ 10MB
        return max(base_rounds // 5, 10)
    elif payload_size <= 100 * 1024**2: # ≤ 100MB
        return max(base_rounds // 20, 5)
    elif payload_size <= 1024**3:       # ≤ 1GB
        return max(base_rounds // 50, 3)
    else:                               # > 1GB
        return max(base_rounds // 100, 2)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_counter = 0
_lock = threading.Lock()


def _next_id():
    global _counter
    with _lock:
        _counter += 1
        return _counter


def _make_payload(size: int) -> bytes:
    """Create a payload of given size. Uses a repeated pattern for large sizes
    (faster than os.urandom) but validates first+last bytes for integrity."""
    if size <= 1024**2:
        return os.urandom(size)
    # For large payloads, use a repeating pattern — much faster to generate
    chunk = os.urandom(min(size, 1024 * 1024))
    repeats = size // len(chunk)
    remainder = size % len(chunk)
    return chunk * repeats + chunk[:remainder]


def _get_rss_mb() -> float:
    """Get current process RSS in MB (macOS/Linux)."""
    try:
        ru = resource.getrusage(resource.RUSAGE_SELF)
        if sys.platform == 'darwin':
            return ru.ru_maxrss / 1024 / 1024  # macOS: bytes
        return ru.ru_maxrss / 1024              # Linux: KB
    except Exception:
        return 0.0


def _start_server(protocol: str) -> tuple:
    """Start a Bench server on the given protocol, return (address, server)."""
    idx = _next_id()
    if protocol == 'thread':
        address = f'thread://bench_{idx}'
    elif protocol == 'memory':
        address = f'memory://bench_{idx}'
    else:
        raise ValueError(f'Unknown protocol: {protocol}')

    config = cc.rpc.ServerConfig(
        name=f'Bench_{protocol}_{idx}',
        crm=Bench(),
        icrm=IBench,
        bind_address=address,
    )
    server = cc.rpc.Server(config)
    _start(server._state)

    for _ in range(100):
        try:
            if cc.rpc.Client.ping(address, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.05)

    return address, server


def _shutdown_server(address: str, server):
    try:
        cc.rpc.Client.shutdown(address, timeout=5.0)
    except Exception:
        pass
    time.sleep(0.2)
    try:
        server.stop()
    except Exception:
        pass


def _percentile(data: list[float], p: float) -> float:
    if not data:
        return 0.0
    k = (len(data) - 1) * p / 100.0
    f = int(k)
    c = f + 1
    if c >= len(data):
        return data[f]
    return data[f] + (k - f) * (data[c] - data[f])


# ---------------------------------------------------------------------------
# Benchmark runners
# ---------------------------------------------------------------------------

def run_latency_test(protocol: str, payload_size: int, rounds: int) -> dict:
    """Measure per-call latency over `rounds` round-trips."""
    address, server = _start_server(protocol)
    payload = _make_payload(payload_size)

    latencies = []
    rss_before = _get_rss_mb()

    try:
        with cc.compo.runtime.connect_crm(address, IBench) as bench:
            # Warm up
            warmup_n = min(3, rounds)
            for _ in range(warmup_n):
                bench.echo(payload)

            for i in range(rounds):
                t0 = time.perf_counter()
                result = bench.echo(payload)
                t1 = time.perf_counter()
                latencies.append((t1 - t0) * 1000)

                assert len(result) == payload_size, (
                    f'Data corruption at round {i}: expected {payload_size}, got {len(result)}'
                )
    finally:
        _shutdown_server(address, server)

    rss_after = _get_rss_mb()
    latencies.sort()

    total_data_bytes = payload_size * rounds * 2  # send + receive

    return {
        'protocol': protocol,
        'payload_bytes': payload_size,
        'payload_human': human_size(payload_size),
        'rounds': rounds,
        'min_ms': round(latencies[0], 4),
        'mean_ms': round(statistics.mean(latencies), 4),
        'p50_ms': round(_percentile(latencies, 50), 4),
        'p95_ms': round(_percentile(latencies, 95), 4),
        'p99_ms': round(_percentile(latencies, 99), 4),
        'max_ms': round(latencies[-1], 4),
        'stdev_ms': round(statistics.stdev(latencies), 4) if len(latencies) > 1 else 0,
        'total_data_MB': round(total_data_bytes / 1024**2, 2),
        'effective_bandwidth_MB_s': round(
            total_data_bytes / (sum(latencies) / 1000) / 1024**2, 2
        ) if sum(latencies) > 0 else 0,
        'rss_delta_MB': round(rss_after - rss_before, 1),
    }


def run_throughput_test(protocol: str, payload_size: int, duration_sec: float) -> dict:
    """Measure sustained throughput (ops/sec) over `duration_sec` seconds."""
    # For very large payloads, limit duration to avoid extremely long runs
    effective_duration = min(duration_sec, max(3.0, 30.0 - payload_size / 1024**3 * 20))

    address, server = _start_server(protocol)
    payload = _make_payload(payload_size)

    ops = 0
    total_bytes = 0
    try:
        with cc.compo.runtime.connect_crm(address, IBench) as bench:
            for _ in range(2):
                bench.echo(payload)

            t_start = time.perf_counter()
            deadline = t_start + effective_duration
            while time.perf_counter() < deadline:
                bench.echo(payload)
                ops += 1
                total_bytes += payload_size * 2
            t_end = time.perf_counter()
    finally:
        _shutdown_server(address, server)

    elapsed = t_end - t_start
    return {
        'protocol': protocol,
        'payload_bytes': payload_size,
        'payload_human': human_size(payload_size),
        'duration_sec': round(elapsed, 2),
        'total_ops': ops,
        'ops_per_sec': round(ops / elapsed, 1) if elapsed > 0 else 0,
        'throughput_MB_per_sec': round(total_bytes / elapsed / 1024**2, 2) if elapsed > 0 else 0,
        'total_data_MB': round(total_bytes / 1024**2, 2),
    }


# ---------------------------------------------------------------------------
# Output formatting
# ---------------------------------------------------------------------------

def print_latency_table(results: list[dict]):
    print('\n' + '=' * 110)
    print('LATENCY TEST (ms)')
    print('=' * 110)
    header = (
        f'{"Protocol":<10} {"Payload":<10} {"Rounds":<7} '
        f'{"Min":<10} {"P50":<10} {"P95":<10} {"P99":<10} {"Max":<10} '
        f'{"BW(MB/s)":<10} {"DataTx":<10}'
    )
    print(header)
    print('-' * 110)
    for r in results:
        print(
            f'{r["protocol"]:<10} '
            f'{r["payload_human"]:<10} '
            f'{r["rounds"]:<7} '
            f'{r["min_ms"]:<10.3f} '
            f'{r["p50_ms"]:<10.3f} '
            f'{r["p95_ms"]:<10.3f} '
            f'{r["p99_ms"]:<10.3f} '
            f'{r["max_ms"]:<10.3f} '
            f'{r["effective_bandwidth_MB_s"]:<10.1f} '
            f'{human_size(int(r["total_data_MB"] * 1024**2)):<10}'
        )
    print()


def print_throughput_table(results: list[dict]):
    print('=' * 90)
    print('THROUGHPUT TEST')
    print('=' * 90)
    header = f'{"Protocol":<10} {"Payload":<10} {"Duration":<10} {"Ops":<8} {"Ops/s":<12} {"MB/s":<10} {"DataTx":<10}'
    print(header)
    print('-' * 90)
    for r in results:
        print(
            f'{r["protocol"]:<10} '
            f'{r["payload_human"]:<10} '
            f'{r["duration_sec"]:<10.1f} '
            f'{r["total_ops"]:<8} '
            f'{r["ops_per_sec"]:<12.1f} '
            f'{r["throughput_MB_per_sec"]:<10.2f} '
            f'{human_size(int(r["total_data_MB"] * 1024**2)):<10}'
        )
    print()


def print_scaling_analysis(results: list[dict], protocols: list[str]):
    """Analyze how latency scales with payload size — separate fixed overhead from data cost."""
    print('=' * 100)
    print('SCALING ANALYSIS — Fixed Overhead vs Data-Proportional Cost')
    print('=' * 100)
    print(
        f'{"Protocol":<10} {"Payload":<10} {"P50(ms)":<10} '
        f'{"Δ from min":<12} {"Per-MB(ms)":<12} {"BW(MB/s)":<12} {"Overhead%":<10}'
    )
    print('-' * 100)

    for proto in protocols:
        proto_results = sorted(
            [r for r in results if r['protocol'] == proto],
            key=lambda r: r['payload_bytes'],
        )
        if not proto_results:
            continue

        # Smallest payload latency ≈ fixed overhead
        baseline_ms = proto_results[0]['p50_ms']

        for r in proto_results:
            size = r['payload_bytes']
            p50 = r['p50_ms']
            delta = p50 - baseline_ms
            # Per-MB cost: (p50 - baseline) / payload_MB, for payloads > baseline size
            payload_mb = size / 1024**2
            per_mb_ms = (delta / payload_mb) if payload_mb > 0.001 and delta > 0 else 0
            bw = r['effective_bandwidth_MB_s']
            overhead_pct = (baseline_ms / p50 * 100) if p50 > 0 else 0

            print(
                f'{r["protocol"]:<10} '
                f'{r["payload_human"]:<10} '
                f'{p50:<10.3f} '
                f'{delta:<12.3f} '
                f'{per_mb_ms:<12.2f} '
                f'{bw:<12.1f} '
                f'{overhead_pct:<10.1f}'
            )
        print()

    print('Interpretation:')
    print('  - "Δ from min" = data-proportional latency (P50 minus smallest-payload P50)')
    print('  - "Per-MB(ms)" = marginal cost of each additional MB of payload')
    print('  - "Overhead%"  = what fraction of total latency is fixed overhead')
    print('  - When Overhead% is high → bottleneck is protocol machinery (polling, file I/O)')
    print('  - When Overhead% is low  → bottleneck is data transfer (serialization, I/O bandwidth)')
    print()


def print_comparison(results: list[dict], payload_sizes: list[int]):
    """Side-by-side memory vs thread comparison."""
    print('=' * 80)
    print('COMPARISON: memory / thread ratio')
    print('=' * 80)
    print(f'{"Payload":<10} {"thread P50":<12} {"memory P50":<12} {"Ratio":<10} {"BW thread":<12} {"BW memory":<12}')
    print('-' * 80)

    for size in payload_sizes:
        thread_r = next(
            (r for r in results if r['protocol'] == 'thread' and r['payload_bytes'] == size),
            None,
        )
        memory_r = next(
            (r for r in results if r['protocol'] == 'memory' and r['payload_bytes'] == size),
            None,
        )
        if thread_r and memory_r:
            ratio = memory_r['p50_ms'] / thread_r['p50_ms'] if thread_r['p50_ms'] > 0 else float('inf')
            print(
                f'{human_size(size):<10} '
                f'{thread_r["p50_ms"]:<12.3f} '
                f'{memory_r["p50_ms"]:<12.3f} '
                f'{ratio:<10.1f}x '
                f'{thread_r["effective_bandwidth_MB_s"]:<12.1f} '
                f'{memory_r["effective_bandwidth_MB_s"]:<12.1f}'
            )
    print()


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description='Memory RPC Benchmark — latency & throughput across payload sizes',
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Presets:
  quick     64B, 1KB, 64KB, 1MB                       (~30s)
  standard  64B, 1KB, 64KB, 1MB, 10MB, 100MB          (~3min)
  full      64B → 1GB                                  (~15min)
  stress    64B → 4GB                                  (~30min+)

Examples:
  %(prog)s --preset standard
  %(prog)s --payloads 64B,10MB,500MB,2GB --rounds-override 3
  %(prog)s --protocols memory --payloads 1GB --skip-throughput
        """,
    )
    parser.add_argument('--preset', choices=['quick', 'standard', 'full', 'stress'],
                        default=None,
                        help='Predefined payload size sets (default: standard)')
    parser.add_argument('--rounds', type=int, default=100,
                        help='Base rounds for latency test; auto-scaled for large payloads (default: 100)')
    parser.add_argument('--rounds-override', type=int, default=None,
                        help='Force exact round count for ALL sizes (disables auto-scaling)')
    parser.add_argument('--duration', type=float, default=5.0,
                        help='Duration in seconds for throughput test (default: 5.0)')
    parser.add_argument('--payloads', type=str, default=None,
                        help='Comma-separated payload sizes with units, e.g. "64B,10MB,1GB"')
    parser.add_argument('--protocols', type=str, default='thread,memory',
                        help='Comma-separated protocols to test (default: thread,memory)')
    parser.add_argument('--output', type=str, default=None,
                        help='Output JSON file path (optional)')
    parser.add_argument('--skip-throughput', action='store_true',
                        help='Skip throughput test')
    parser.add_argument('--skip-latency', action='store_true',
                        help='Skip latency test')
    args = parser.parse_args()

    # Resolve payload sizes
    if args.payloads:
        payload_sizes = [parse_size(x) for x in args.payloads.split(',')]
    elif args.preset:
        payload_sizes = PRESETS[args.preset]
    else:
        payload_sizes = PRESETS['standard']

    protocols = [x.strip() for x in args.protocols.split(',')]

    # Summary
    print(f'\n{"=" * 60}')
    print(f'  Memory RPC Benchmark')
    print(f'{"=" * 60}')
    print(f'  Protocols : {protocols}')
    print(f'  Payloads  : {[human_size(s) for s in payload_sizes]}')
    print(f'  Max data  : {human_size(max(payload_sizes))} per call')
    total_data_est = sum(
        s * 2 * (args.rounds_override or auto_rounds(s, args.rounds))
        for s in payload_sizes
    ) * len(protocols)
    print(f'  Est. data : ~{human_size(total_data_est)} total transfer (latency test)')
    print(f'  Base rounds: {args.rounds} (auto-scaled by size)')
    if args.rounds_override:
        print(f'  Override  : {args.rounds_override} rounds for ALL sizes')
    print(f'  RSS now   : {_get_rss_mb():.0f} MB')
    print(f'{"=" * 60}\n')

    all_results = {'latency': [], 'throughput': [], 'meta': {
        'protocols': protocols,
        'payload_sizes': payload_sizes,
        'base_rounds': args.rounds,
        'rounds_override': args.rounds_override,
        'platform': sys.platform,
        'timestamp': time.strftime('%Y-%m-%dT%H:%M:%S'),
    }}

    # --- Latency tests ---
    if not args.skip_latency:
        for protocol in protocols:
            for size in payload_sizes:
                rounds = args.rounds_override or auto_rounds(size, args.rounds)
                print(
                    f'[Latency] {protocol} @ {human_size(size)} × {rounds} rounds ...',
                    end=' ', flush=True,
                )
                result = run_latency_test(protocol, size, rounds)
                all_results['latency'].append(result)
                print(
                    f'p50={result["p50_ms"]:.3f}ms  '
                    f'p99={result["p99_ms"]:.3f}ms  '
                    f'BW={result["effective_bandwidth_MB_s"]:.1f}MB/s'
                )

        print_latency_table(all_results['latency'])
        print_scaling_analysis(all_results['latency'], protocols)

    # --- Throughput tests ---
    if not args.skip_throughput:
        for protocol in protocols:
            for size in payload_sizes:
                print(
                    f'[Throughput] {protocol} @ {human_size(size)} ...',
                    end=' ', flush=True,
                )
                result = run_throughput_test(protocol, size, args.duration)
                all_results['throughput'].append(result)
                print(
                    f'{result["ops_per_sec"]:.0f} ops/s  '
                    f'{result["throughput_MB_per_sec"]:.1f} MB/s'
                )

        print_throughput_table(all_results['throughput'])

    # --- Comparison ---
    if not args.skip_latency and len(protocols) >= 2 and all_results['latency']:
        print_comparison(all_results['latency'], payload_sizes)

    # --- Save JSON ---
    if args.output:
        with open(args.output, 'w') as f:
            json.dump(all_results, f, indent=2)
        print(f'Results saved to {args.output}')

    print(f'Peak RSS: {_get_rss_mb():.0f} MB')


if __name__ == '__main__':
    main()
