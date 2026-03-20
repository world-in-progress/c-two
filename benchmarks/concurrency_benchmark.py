"""
Concurrency benchmark for C-Two scheduler and Router.

Measures:
  - Exclusive vs read_parallel throughput on thread:// and memory://
  - Router relay overhead vs direct Worker access
  - Backpressure rejection latency under load

Usage:
    uv run python benchmarks/concurrency_benchmark.py [--quick | --full]
"""

import argparse
import queue
import statistics
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / 'src'))

import c_two as cc
from c_two.rpc import (
    ConcurrencyConfig,
    ConcurrencyMode,
    Router,
    RouterConfig,
    Server,
    ServerConfig,
)
from c_two.rpc.server import _start

# ---------------------------------------------------------------------------
# ICRM / CRM for benchmarking
# ---------------------------------------------------------------------------

@cc.icrm(namespace='cc.bench', version='0.1.0')
class IBenchCRM:
    @cc.read
    def read_op(self, payload: bytes) -> bytes:
        ...

    @cc.write
    def write_op(self, payload: bytes) -> bytes:
        ...


class BenchCRM:
    def read_op(self, payload: bytes) -> bytes:
        return payload

    def write_op(self, payload: bytes) -> bytes:
        return payload


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_counter = 0
_counter_lock = threading.Lock()


def _next_id() -> int:
    global _counter
    with _counter_lock:
        _counter += 1
        return _counter


def _make_server(address: str, mode: ConcurrencyMode, max_workers: int = 4, max_pending: int | None = None):
    server = Server(ServerConfig(
        name='BenchCRM',
        crm=BenchCRM(),
        icrm=IBenchCRM,
        bind_address=address,
        concurrency=ConcurrencyConfig(mode=mode, max_workers=max_workers, max_pending=max_pending),
    ))
    _start(server._state)
    for _ in range(50):
        try:
            if cc.rpc.Client.ping(address, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.1)
    return server


def _shutdown_server(address: str, server: Server):
    try:
        cc.rpc.Client.shutdown(address, timeout=2.0)
    except Exception:
        pass
    time.sleep(0.1)
    try:
        server.stop()
    except Exception:
        pass


# ---------------------------------------------------------------------------
# Benchmark: Concurrent reads throughput
# ---------------------------------------------------------------------------

def bench_concurrent_reads(protocol: str, mode: ConcurrencyMode, n_clients: int, rounds: int, payload_size: int):
    address = f'{protocol}://bench_read_{_next_id()}'
    server = _make_server(address, mode, max_workers=n_clients)
    payload = b'\x00' * payload_size

    latencies: list[float] = []
    errors: list[Exception] = []
    barrier = threading.Barrier(n_clients + 1)

    def worker():
        barrier.wait()
        for _ in range(rounds):
            t0 = time.perf_counter()
            try:
                with cc.compo.runtime.connect_crm(address, IBenchCRM) as crm:
                    crm.read_op(payload)
                latencies.append(time.perf_counter() - t0)
            except Exception as exc:
                errors.append(exc)

    threads = [threading.Thread(target=worker) for _ in range(n_clients)]
    for t in threads:
        t.start()

    barrier.wait()
    t_start = time.perf_counter()

    for t in threads:
        t.join()
    elapsed = time.perf_counter() - t_start

    _shutdown_server(address, server)

    total_ops = len(latencies)
    return {
        'protocol': protocol,
        'mode': mode.value,
        'clients': n_clients,
        'rounds_per_client': rounds,
        'payload_bytes': payload_size,
        'total_ops': total_ops,
        'errors': len(errors),
        'elapsed_sec': round(elapsed, 4),
        'ops_per_sec': round(total_ops / elapsed, 1) if elapsed > 0 else 0,
        'p50_ms': round(statistics.median(latencies) * 1000, 3) if latencies else 0,
        'p95_ms': round(sorted(latencies)[int(len(latencies) * 0.95)] * 1000, 3) if latencies else 0,
    }


# ---------------------------------------------------------------------------
# Benchmark: Router relay overhead
# ---------------------------------------------------------------------------

def bench_router_relay(n_clients: int, rounds: int, payload_size: int):
    worker_address = f'thread://bench_router_worker_{_next_id()}'
    router_port = 17500 + _next_id()
    router_url = f'http://127.0.0.1:{router_port}'

    server = _make_server(worker_address, ConcurrencyMode.READ_PARALLEL, max_workers=n_clients)

    router = Router(RouterConfig(bind_address=router_url, max_relay_workers=n_clients))
    router.attach(server)
    router.start(blocking=False)
    time.sleep(0.5)

    namespace = IBenchCRM.__tag__.split('/')[0]
    endpoint = f'{router_url}/{namespace}'
    payload = b'\x00' * payload_size

    latencies: list[float] = []
    errors: list[Exception] = []
    barrier = threading.Barrier(n_clients + 1)

    def worker():
        barrier.wait()
        for _ in range(rounds):
            t0 = time.perf_counter()
            try:
                with cc.compo.runtime.connect_crm(endpoint, IBenchCRM) as crm:
                    crm.read_op(payload)
                latencies.append(time.perf_counter() - t0)
            except Exception as exc:
                errors.append(exc)

    threads = [threading.Thread(target=worker) for _ in range(n_clients)]
    for t in threads:
        t.start()
    barrier.wait()
    t_start = time.perf_counter()
    for t in threads:
        t.join()
    elapsed = time.perf_counter() - t_start

    router.stop()
    _shutdown_server(worker_address, server)

    total_ops = len(latencies)
    return {
        'transport': 'router→thread',
        'clients': n_clients,
        'rounds_per_client': rounds,
        'payload_bytes': payload_size,
        'total_ops': total_ops,
        'errors': len(errors),
        'elapsed_sec': round(elapsed, 4),
        'ops_per_sec': round(total_ops / elapsed, 1) if elapsed > 0 else 0,
        'p50_ms': round(statistics.median(latencies) * 1000, 3) if latencies else 0,
        'p95_ms': round(sorted(latencies)[int(len(latencies) * 0.95)] * 1000, 3) if latencies else 0,
    }


# ---------------------------------------------------------------------------
# Benchmark: Backpressure rejection
# ---------------------------------------------------------------------------

def bench_backpressure(max_pending: int, n_overflow: int):
    address = f'thread://bench_bp_{_next_id()}'
    server = _make_server(address, ConcurrencyMode.READ_PARALLEL, max_workers=max_pending, max_pending=max_pending)

    accepted = 0
    rejected = 0
    barrier = threading.Barrier(max_pending + n_overflow + 1)

    def slow_worker():
        barrier.wait()
        try:
            with cc.compo.runtime.connect_crm(address, IBenchCRM) as crm:
                crm.read_op(b'\x00' * 1024)
        except Exception:
            pass

    def overflow_worker():
        nonlocal accepted, rejected
        barrier.wait()
        time.sleep(0.05)
        try:
            with cc.compo.runtime.connect_crm(address, IBenchCRM) as crm:
                crm.read_op(b'\x00' * 64)
            accepted += 1
        except Exception:
            rejected += 1

    threads = [threading.Thread(target=slow_worker) for _ in range(max_pending)]
    threads += [threading.Thread(target=overflow_worker) for _ in range(n_overflow)]
    for t in threads:
        t.start()
    barrier.wait()
    for t in threads:
        t.join()

    _shutdown_server(address, server)

    return {
        'max_pending': max_pending,
        'overflow_attempts': n_overflow,
        'accepted': accepted,
        'rejected': rejected,
    }


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------

def print_result(label: str, result: dict):
    print(f'\n  {label}:')
    for k, v in result.items():
        print(f'    {k}: {v}')


def run_quick():
    print('=== Quick Concurrency Benchmark ===')

    print('\n--- Exclusive vs Read-Parallel (thread://, 2 clients, 64B) ---')
    r1 = bench_concurrent_reads('thread', ConcurrencyMode.EXCLUSIVE, 2, 50, 64)
    print_result('exclusive', r1)
    r2 = bench_concurrent_reads('thread', ConcurrencyMode.READ_PARALLEL, 2, 50, 64)
    print_result('read_parallel', r2)

    print('\n--- Router Relay (2 clients, 64B) ---')
    r3 = bench_router_relay(2, 20, 64)
    print_result('router→thread', r3)

    print('\n--- Backpressure (max_pending=2, overflow=3) ---')
    r4 = bench_backpressure(2, 3)
    print_result('backpressure', r4)


def run_full():
    print('=== Full Concurrency Benchmark ===')

    for protocol in ['thread']:
        for n_clients in [1, 2, 4, 8]:
            for payload in [64, 1024, 65536]:
                for mode in [ConcurrencyMode.EXCLUSIVE, ConcurrencyMode.READ_PARALLEL]:
                    label = f'{protocol}://{mode.value} c={n_clients} p={payload}B'
                    result = bench_concurrent_reads(protocol, mode, n_clients, 30, payload)
                    print_result(label, result)

    print('\n--- Router Relay ---')
    for n_clients in [1, 2, 4]:
        for payload in [64, 1024]:
            result = bench_router_relay(n_clients, 20, payload)
            print_result(f'router c={n_clients} p={payload}B', result)

    print('\n--- Backpressure ---')
    for max_p in [2, 4]:
        result = bench_backpressure(max_p, max_p + 2)
        print_result(f'max_pending={max_p}', result)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description='C-Two concurrency benchmarks')
    parser.add_argument('--quick', action='store_true', default=True, help='Quick benchmark (default)')
    parser.add_argument('--full', action='store_true', help='Full benchmark matrix')
    args = parser.parse_args()

    if args.full:
        run_full()
    else:
        run_quick()
