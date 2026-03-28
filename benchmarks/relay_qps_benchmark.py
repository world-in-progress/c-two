"""Relay QPS benchmark — measures requests/second through NativeRelay.

Uses `hey` (Go HTTP benchmark tool) for accurate measurements.
Starts relay + CRM in-process, then invokes hey as subprocess.

Outputs: QPS=<number>
"""
from __future__ import annotations

import glob
import os
import re
import subprocess
import sys
import time
import pickle

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import c_two as cc
from c_two._native import NativeRelay
from c_two.rpc_v2.registry import _ProcessRegistry
from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello

# ── Configuration ─────────────────────────────────────────────────────────

RELAY_PORT = 19950 + (os.getpid() % 100)
TOTAL_REQUESTS = 3000
CONCURRENCY = 32

IPC_NAME = f'relay_bench_{os.getpid()}'
IPC_ADDR = f'ipc-v3://{IPC_NAME}'
PAYLOAD_FILE = '/tmp/c2_bench_payload.bin'


def cleanup_stale():
    for f in glob.glob('/tmp/c_two_ipc/relay_bench_*.sock'):
        try:
            os.unlink(f)
        except OSError:
            pass
    try:
        from c_two.buddy import cleanup_stale_shm
        cleanup_stale_shm()
    except Exception:
        pass


def main():
    cleanup_stale()
    _ProcessRegistry.reset()

    # Write payload file for hey
    with open(PAYLOAD_FILE, 'wb') as f:
        f.write(pickle.dumps(('Benchmark',)))

    try:
        cc.set_address(IPC_ADDR)
        cc.register(IHello, Hello(), name='hello')

        relay = NativeRelay(f'127.0.0.1:{RELAY_PORT}')
        relay.start()
        time.sleep(0.1)

        try:
            relay.register_upstream('hello', IPC_ADDR)

            # Warmup with hey
            subprocess.run(
                ['hey', '-n', '500', '-c', '8', '-m', 'POST',
                 '-D', PAYLOAD_FILE, '-T', 'application/octet-stream',
                 f'http://127.0.0.1:{RELAY_PORT}/hello/greeting'],
                capture_output=True,
            )

            # Benchmark
            result = subprocess.run(
                ['hey', '-n', str(TOTAL_REQUESTS), '-c', str(CONCURRENCY),
                 '-m', 'POST', '-D', PAYLOAD_FILE,
                 '-T', 'application/octet-stream',
                 f'http://127.0.0.1:{RELAY_PORT}/hello/greeting'],
                capture_output=True, text=True,
            )

            # Extract QPS
            m = re.search(r'Requests/sec:\s+([\d.]+)', result.stdout)
            if m:
                qps = float(m.group(1))
                print(f'QPS={qps:.1f}')
            else:
                print('QPS=0.0')
                print(result.stdout[-500:])

            # Extract latency
            for line in result.stdout.splitlines():
                line = line.strip()
                if any(k in line for k in ['Average:', 'Fastest:', 'Slowest:', '50%', '99%', 'Status']):
                    print(f'  {line}')

        finally:
            relay.stop()
    finally:
        cc.shutdown()
        _ProcessRegistry.reset()
        cleanup_stale()
        try:
            os.unlink(PAYLOAD_FILE)
        except OSError:
            pass


if __name__ == '__main__':
    main()
