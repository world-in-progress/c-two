"""Relay QPS benchmark - measures requests/second through an external c3 relay.

Uses `hey` (Go HTTP benchmark tool) for accurate measurements.
Starts the CRM in-process, requires an already-running standalone relay, then
invokes hey as subprocess.

Start the relay first from a source checkout:

    python tools/dev/c3_tool.py --build --link
    c3 relay --bind 127.0.0.1:<port>

Outputs: QPS=<number>
"""
from __future__ import annotations

import os
import pickle
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import c_two as cc
import _relay_support as relay
from c_two.crm.contract import CRMContract, crm_contract
from c_two.transport.registry import _reset_default_runtime
from tests.fixtures.hello import HelloImpl
from tests.fixtures.ihello import Hello

# ── Configuration ─────────────────────────────────────────────────────────

RELAY_PORT = 19950 + (os.getpid() % 100)
TOTAL_REQUESTS = 3000
CONCURRENCY = 32

PAYLOAD_FILE = '/tmp/c2_bench_payload.bin'


def _route_hey_args(expected: CRMContract, route_uid: str, route_revision: int) -> list[str]:
    return [
        '-H',
        f'x-c2-expected-crm-ns: {expected.crm_ns}',
        '-H',
        f'x-c2-expected-crm-name: {expected.crm_name}',
        '-H',
        f'x-c2-expected-crm-ver: {expected.crm_ver}',
        '-H',
        f'x-c2-expected-abi-hash: {expected.abi_hash}',
        '-H',
        f'x-c2-expected-signature-hash: {expected.signature_hash}',
        '-H',
        f'x-c2-route-uid: {route_uid}',
        '-H',
        f'x-c2-route-revision: {route_revision}',
    ]


def _stdout_tail(stdout: str, limit: int = 1200) -> str:
    return stdout[-limit:] if stdout else '<empty>'


def _run_hey(label: str, args: list[str]) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(args, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(
            f'hey {label} failed with exit code {result.returncode}\n'
            f'stderr:\n{result.stderr or "<empty>"}\n'
            f'stdout tail:\n{_stdout_tail(result.stdout)}'
        )
    if 'Status code distribution:' in result.stdout:
        ok = re.search(r'\[2\d\d\]\s+\d+\s+responses', result.stdout)
        if not ok:
            raise RuntimeError(
                f'hey {label} did not receive any 2xx responses\n'
                f'stderr:\n{result.stderr or "<empty>"}\n'
                f'stdout tail:\n{_stdout_tail(result.stdout)}'
            )
    return result


def cleanup_stale():
    try:
        from c_two.mem import cleanup_stale_shm
        cleanup_stale_shm()
    except Exception:
        pass


def main():
    cleanup_stale()
    _reset_default_runtime()
    relay_url = f'http://127.0.0.1:{RELAY_PORT}'
    route_name = f'hello_bench_{os.getpid()}'

    try:
        with relay.session(relay_url):
            with open(PAYLOAD_FILE, 'wb') as f:
                f.write(pickle.dumps('Benchmark'))
            try:
                cc.register(Hello, HelloImpl(), name=route_name)
                with cc.connect(Hello, name=route_name, address=relay_url) as hello:
                    expected_header_args = _route_hey_args(
                        crm_contract(Hello), hello.client.route_uid, hello.client.route_revision,
                    )

                # Warmup with hey
                _run_hey(
                    'warmup',
                    ['hey', '-n', '500', '-c', '8', '-m', 'POST',
                     '-D', PAYLOAD_FILE, '-T', 'application/octet-stream',
                     *expected_header_args,
                     f'{relay_url}/{route_name}/greeting'],
                )

                # Benchmark
                result = _run_hey(
                    'benchmark',
                    ['hey', '-n', str(TOTAL_REQUESTS), '-c', str(CONCURRENCY),
                     '-m', 'POST', '-D', PAYLOAD_FILE,
                     '-T', 'application/octet-stream',
                     *expected_header_args,
                     f'{relay_url}/{route_name}/greeting'],
                )

                # Extract QPS
                m = re.search(r'Requests/sec:\s+([\d.]+)', result.stdout)
                if m:
                    qps = float(m.group(1))
                    print(f'QPS={qps:.1f}')
                else:
                    raise RuntimeError(
                        'hey benchmark output did not include Requests/sec\n'
                        f'stderr:\n{result.stderr or "<empty>"}\n'
                        f'stdout tail:\n{_stdout_tail(result.stdout)}'
                    )

                # Extract latency
                for line in result.stdout.splitlines():
                    line = line.strip()
                    if any(k in line for k in ['Average:', 'Fastest:', 'Slowest:', '50%', '99%', 'Status']):
                        print(f'  {line}')
            finally:
                try:
                    os.unlink(PAYLOAD_FILE)
                except OSError:
                    pass
    finally:
        cleanup_stale()


if __name__ == '__main__':
    main()
