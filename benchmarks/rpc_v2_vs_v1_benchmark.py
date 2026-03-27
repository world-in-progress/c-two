"""Benchmark: rpc_v2 (ServerV2 + SharedClient) vs old rpc (Server + IPCv3Client).

Uses @transferable Payload wrapper to avoid the raw bytes fast-path optimization.
100 rounds per size, P50 latency comparison.

Usage:
    uv run python benchmarks/rpc_v2_vs_v1_benchmark.py
"""
from __future__ import annotations

import gc
import math
import os
import statistics
import sys
import time
import uuid

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))

import c_two as cc
from c_two.rpc.server import Server, ServerConfig, _start
from c_two.rpc.ipc.ipc_protocol import IPCConfig
from c_two.rpc_v2 import ServerV2, ICRMProxy
from c_two.rpc_v2.client import SharedClient


# ---------------------------------------------------------------------------
# Benchmark CRM definitions — Payload wrapper avoids bytes fast-path
# ---------------------------------------------------------------------------

@cc.transferable
class Payload:
    """Wraps raw bytes to avoid the raw-bytes fast-path optimization."""
    data: bytes

    def serialize(p: 'Payload') -> bytes:
        return p.data

    def deserialize(raw: bytes) -> 'Payload':
        return Payload(data=bytes(raw) if isinstance(raw, memoryview) else raw)


@cc.icrm(namespace='bench.payload', version='0.1.0')
class IEcho:
    def echo(self, payload: Payload) -> Payload: ...


class Echo:
    def echo(self, payload: Payload) -> Payload:
        return payload


# ---------------------------------------------------------------------------
# Test matrix
# ---------------------------------------------------------------------------

SIZES = [
    (64,                    '64B'),
    (256,                   '256B'),
    (1024,                  '1KB'),
    (4 * 1024,              '4KB'),
    (16 * 1024,             '16KB'),
    (64 * 1024,             '64KB'),
    (256 * 1024,            '256KB'),
    (1024 * 1024,           '1MB'),
    (4 * 1024 * 1024,       '4MB'),
    (10 * 1024 * 1024,      '10MB'),
    (50 * 1024 * 1024,      '50MB'),
    (100 * 1024 * 1024,     '100MB'),
    (500 * 1024 * 1024,     '500MB'),
    (1024 * 1024 * 1024,    '1GB'),
]

ROUNDS = 100
WARMUP = 5

_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')


def _ipc_config() -> IPCConfig:
    return IPCConfig(
        pool_enabled=True,
        pool_segment_size=2 * 1024 * 1024 * 1024,  # 2 GB
        max_pool_segments=8,
        max_pool_memory=4 * 1024 * 1024 * 1024,
    )


def _wait_old_server(address: str, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if cc.rpc.Client.ping(address, timeout=0.5):
                return
        except Exception:
            pass
        time.sleep(0.05)
    raise RuntimeError(f'Old server at {address} not ready')


def _wait_v2_server(addr: str, timeout: float = 5.0) -> None:
    region_id = addr.replace('ipc-v3://', '')
    sock_path = os.path.join(_IPC_SOCK_DIR, f'{region_id}.sock')
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.exists(sock_path):
            return
        time.sleep(0.05)
    raise RuntimeError(f'ServerV2 at {sock_path} not ready')


# ---------------------------------------------------------------------------
# Old rpc path — Server + connect_crm (IPCv3Client internally)
# ---------------------------------------------------------------------------

def bench_old(payload_size: int) -> float:
    uid = uuid.uuid4().hex[:8]
    address = f'ipc-v3://bench_old_{uid}'
    ipc_config = _ipc_config()

    server = Server(ServerConfig(
        name='BenchOld',
        crm=Echo(),
        icrm=IEcho,
        bind_address=address,
        ipc_config=ipc_config,
    ))
    _start(server._state)
    _wait_old_server(address)

    payload = Payload(data=b'\xAB' * payload_size)
    latencies: list[float] = []

    try:
        with cc.compo.runtime.connect_crm(address, IEcho, ipc_config=ipc_config) as crm:
            for _ in range(WARMUP):
                crm.echo(payload)

            gc.disable()
            try:
                for _ in range(ROUNDS):
                    t0 = time.perf_counter()
                    result = crm.echo(payload)
                    elapsed = time.perf_counter() - t0
                    latencies.append(elapsed)
            finally:
                gc.enable()

            assert isinstance(result, Payload)
            assert len(result.data) == payload_size
    finally:
        try:
            cc.rpc.Client.shutdown(address, timeout=2.0)
        except Exception:
            pass
        server.stop()

    return statistics.median(latencies) * 1000


# ---------------------------------------------------------------------------
# New rpc_v2 path — ServerV2 + SharedClient + ICRMProxy
# ---------------------------------------------------------------------------

def bench_new(payload_size: int) -> float:
    uid = uuid.uuid4().hex[:8]
    address = f'ipc-v3://bench_new_{uid}'
    ipc_config = _ipc_config()

    server = ServerV2(bind_address=address, ipc_config=ipc_config)
    server.register_crm(IEcho, Echo(), name='echo')
    server.start()
    _wait_v2_server(address)

    payload = Payload(data=b'\xAB' * payload_size)
    latencies: list[float] = []

    try:
        client = SharedClient(address, try_v2=True, ipc_config=ipc_config)
        client.connect()
        proxy = ICRMProxy.ipc(client, 'echo')
        icrm = IEcho()
        icrm.client = proxy

        for _ in range(WARMUP):
            icrm.echo(payload)

        gc.disable()
        try:
            for _ in range(ROUNDS):
                t0 = time.perf_counter()
                result = icrm.echo(payload)
                elapsed = time.perf_counter() - t0
                latencies.append(elapsed)
        finally:
            gc.enable()

        assert isinstance(result, Payload)
        assert len(result.data) == payload_size

        client.terminate()
    finally:
        server.shutdown()

    return statistics.median(latencies) * 1000


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    print(f'Benchmark: rpc_v2 vs old rpc (IPCv3)')
    print(f'Rounds: {ROUNDS}, Warmup: {WARMUP}')
    print(f'Payload: @transferable Payload wrapper (no bytes fast-path)')
    print()
    print(f'{"Size":>8}  {"Old P50 (ms)":>14}  {"New P50 (ms)":>14}  {"Δ%":>8}')
    print('-' * 52)

    old_p50s: list[float] = []
    new_p50s: list[float] = []

    for size, label in SIZES:
        sys.stdout.write(f'{label:>8}  ')
        sys.stdout.flush()

        p50_old = bench_old(size)
        sys.stdout.write(f'{p50_old:>14.3f}  ')
        sys.stdout.flush()

        p50_new = bench_new(size)
        delta_pct = (p50_new - p50_old) / p50_old * 100 if p50_old > 0 else 0
        print(f'{p50_new:>14.3f}  {delta_pct:>+7.1f}%')

        old_p50s.append(p50_old)
        new_p50s.append(p50_new)

    # Geometric means
    gmean_old = math.exp(sum(math.log(p) for p in old_p50s) / len(old_p50s))
    gmean_new = math.exp(sum(math.log(p) for p in new_p50s) / len(new_p50s))
    delta = (gmean_new - gmean_old) / gmean_old * 100
    print('-' * 52)
    print(f'{"GeoMean":>8}  {gmean_old:>14.3f}  {gmean_new:>14.3f}  {delta:>+7.1f}%')


if __name__ == '__main__':
    main()
