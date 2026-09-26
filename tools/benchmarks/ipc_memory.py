"""Measure isolated, real IPC calls without treating mapping capacity as RSS.

Run with the candidate environment's Python. By default, worker processes
connect to one shared server in the controller. The pairs topology gives each
worker its own server. Explicit ipc:// addresses exclude direct dispatch.
Run separate invocations for cold/small and large payloads.
RSS is a process high-water mark, including Python and serialization; it is
not an allocator byte counter. Windows reports null when resource is absent.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import time


def peak_rss_bytes() -> int | None:
    try:
        import resource
    except ImportError:
        return None
    rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return int(rss if sys.platform == 'darwin' else rss * 1024)


def echo_types(cc):
    @cc.crm(namespace='c_two.benchmark.memory', version='1.0.0')
    class Echo:
        def echo(self, value: bytes) -> bytes: ...

    class EchoResource:
        def echo(self, value: bytes) -> bytes:
            return value

    return Echo, EchoResource


def configure(cc, args: argparse.Namespace) -> dict:
    overrides = {
        'pool_enabled': args.pool_enabled,
        'pool_segment_size': args.segment_size,
        'max_pool_segments': 2,
    }
    overrides.update(args.ipc_overrides)
    cc.set_transport_policy(shm_threshold=args.shm_threshold)
    cc.set_server(ipc_overrides=overrides)
    cc.set_client(ipc_overrides=overrides)
    return overrides


def measure(args: argparse.Namespace) -> dict:
    os.environ['C2_ENV_FILE'] = ''
    os.environ['C2_RELAY_ANCHOR_ADDRESS'] = ''
    import c_two as cc
    from c_two import _native

    Echo, EchoResource = echo_types(cc)
    overrides = configure(cc, args)
    rss = {'imported': peak_rss_bytes()}
    client = None
    started = time.perf_counter_ns()
    try:
        address = args.address
        if address is None:
            cc.register(Echo, EchoResource(), name='echo')
            rss['registered'] = peak_rss_bytes()
            address = cc.server_address()
        client = cc.connect(Echo, name='echo', address=address)
        connected_ns = time.perf_counter_ns() - started
        rss['connected'] = peak_rss_bytes()
        payload = b'm' * args.payload_bytes
        latencies = []
        for _ in range(args.calls):
            before = time.perf_counter_ns()
            result = client.echo(payload)
            elapsed = time.perf_counter_ns() - before
            if result != payload:
                raise RuntimeError('IPC payload mismatch')
            latencies.append(elapsed)
        rss['calls_completed'] = peak_rss_bytes()
        held = cc.hold(client.echo)(payload)
        try:
            if held.value != payload:
                raise RuntimeError('held IPC payload mismatch')
            held_stats = cc.hold_stats()
        finally:
            held.release()
        after_release = cc.hold_stats()
        if after_release['active_holds'] != 0:
            raise RuntimeError('benchmark retained a transport hold')
    finally:
        if client is not None:
            cc.close(client)
        cc.shutdown()

    ordered = sorted(latencies)
    native_digest = hashlib.sha256()
    with Path(_native.__file__).open('rb') as native_file:
        for block in iter(lambda: native_file.read(1 << 20), b''):
            native_digest.update(block)
    return {
        'pid': os.getpid(),
        'version': importlib.metadata.version('c-two'),
        'fastdb_version': importlib.metadata.version('fastdb4py'),
        'native_path': _native.__file__,
        'native_sha256': native_digest.hexdigest(),
        'python': sys.version,
        'platform': sys.platform,
        'overrides': overrides,
        'shm_threshold': args.shm_threshold,
        'payload_bytes': args.payload_bytes,
        'calls': args.calls,
        'connect_ns': connected_ns,
        'first_call_ns': latencies[0],
        'median_call_ns': statistics.median(latencies),
        'p95_call_ns': ordered[min(len(ordered) - 1, (95 * len(ordered) + 99) // 100 - 1)],
        'rpc_elapsed_ns': sum(latencies),
        'rss_high_water_bytes': rss,
        'held': held_stats,
        'after_release': after_release,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--pool-enabled', action=argparse.BooleanOptionalAction, default=True)
    parser.add_argument('--segment-size', type=int, default=2 * 1024 * 1024)
    parser.add_argument('--payload-bytes', type=int, default=1)
    parser.add_argument('--shm-threshold', type=int, default=4096)
    parser.add_argument('--calls', type=int, default=200)
    parser.add_argument('--workers', type=int, default=1)
    parser.add_argument('--topology', choices=['shared', 'pairs'], default='shared')
    parser.add_argument('--ipc-overrides', type=json.loads, default={}, help='Additional native IPC overrides as a JSON object')
    parser.add_argument('--output', type=Path)
    parser.add_argument('--worker', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--address', help=argparse.SUPPRESS)
    args = parser.parse_args()
    if not isinstance(args.ipc_overrides, dict):
        parser.error('--ipc-overrides must be a JSON object')
    if min(args.calls, args.workers, args.segment_size, args.shm_threshold) <= 0 or args.payload_bytes < 0:
        parser.error('counts and sizes must be positive; payload may be empty')
    if args.worker:
        print(json.dumps(measure(args)))
        return

    command = [
        sys.executable, str(Path(__file__).resolve()), '--worker',
        '--pool-enabled' if args.pool_enabled else '--no-pool-enabled',
        '--segment-size', str(args.segment_size),
        '--payload-bytes', str(args.payload_bytes),
        '--shm-threshold', str(args.shm_threshold), '--calls', str(args.calls),
        '--ipc-overrides', json.dumps(args.ipc_overrides),
    ]
    children: list[subprocess.Popen] = []
    results = []
    server_cc = None
    server_stats = None
    try:
        if args.topology == 'shared':
            os.environ['C2_ENV_FILE'] = ''
            os.environ['C2_RELAY_ANCHOR_ADDRESS'] = ''
            import c_two as server_cc

            Echo, EchoResource = echo_types(server_cc)
            configure(server_cc, args)
            server_cc.register(Echo, EchoResource(), name='echo')
            command.extend(['--address', server_cc.server_address()])
            server_stats = {
                'pid': os.getpid(),
                'rss_after_register_high_water_bytes': peak_rss_bytes(),
            }
        for _ in range(args.workers):
            children.append(subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True))
        for child in children:
            stdout, stderr = child.communicate(timeout=120)
            if child.returncode:
                raise RuntimeError(f'benchmark child {child.pid} failed: {stderr}')
            results.append(json.loads(stdout))
        if server_stats is not None:
            server_stats['rss_after_calls_high_water_bytes'] = peak_rss_bytes()
    finally:
        for child in children:
            if child.poll() is None:
                child.kill()
                child.communicate()
        if server_cc is not None:
            server_cc.shutdown()

    report = {
        'schema': 'c-two.ipc-memory-benchmark.v1',
        'note': 'RSS is per-process peak, not summed shared backing or allocator usage; no baseline improvement is inferred.',
        'topology': args.topology,
        'server': server_stats,
        'workers': results,
    }
    text = json.dumps(report, indent=2) + '\n'
    if args.output:
        args.output.write_text(text)
    print(text, end='')


if __name__ == '__main__':
    main()
