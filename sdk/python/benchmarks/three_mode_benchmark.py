"""Benchmark: Thread-local vs IPC vs Relay (HTTP) — payload 64B to 1GB.

Measures P50 round-trip latency for echo across three transport modes:
  - Thread-local: same process, zero serialization
  - IPC: full serialize + SHM + UDS
  - Relay: HTTP -> external c3 relay -> IPC -> CRM -> reverse

Two payload types compared:
  - bytes: identity fast path (skips pickle)
  - dict:  full pickle serialization (realistic workload)

Payload sizes: 64B, 256B, 1KB, 4KB, 64KB, 1MB, 10MB, 50MB, 100MB, 500MB, 1GB

Results are written to sdk/python/benchmarks/results/ (git-ignored).

Usage:
    C2_RELAY_ANCHOR_ADDRESS= uv run python sdk/python/benchmarks/three_mode_benchmark.py
    C2_RELAY_ANCHOR_ADDRESS= uv run python sdk/python/benchmarks/three_mode_benchmark.py --segment-size 268435456
"""
from __future__ import annotations

import argparse
import gc
import json
import math
import os
import statistics
import sys
import time
import urllib.error
import urllib.request

import c_two as cc
from c_two.config.ipc import _resolve_server_ipc_config
from c_two.crm.contract import CRMContract, crm_contract
from c_two.transport.registry import _ProcessRegistry

# Configurable via CLI --segment-size (bytes)
_SEGMENT_SIZE: int = 2 * 1024 * 1024 * 1024  # default 2GB
_MAX_SEGMENTS: int = 8

RELAY_CLIENT_REQUEST_BODY_PATH = 'materialized'
RELAY_RELAY_REQUEST_BODY_PATH = 'streamed_to_upstream_ipc'
RELAY_RESPONSE_BODY_PATH = 'materialized'
RELAY_TRANSPORT_LABEL = 'relay_http_request_stream_to_shm_response_materialized'

# ---------------------------------------------------------------------------
# Echo CRMs — bytes (identity fast path) vs dict (pickle path)
# ---------------------------------------------------------------------------

@cc.crm(namespace='bench.three_mode', version='0.1.0')
class Echo:
    def echo(self, data: bytes) -> bytes: ...

class EchoImpl:
    def echo(self, data: bytes) -> bytes:
        return data


@cc.crm(namespace='bench.three_mode_dict', version='0.1.0')
class DictEcho:
    def echo(self, data: dict[str, bytes]) -> dict[str, bytes]: ...

class DictEchoImpl:
    def echo(self, data: dict[str, bytes]) -> dict[str, bytes]:
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
_ipc_counter = 0
_relay_port = 19960 + (os.getpid() % 100)

_NO_PROXY_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def _relay_help(relay_url: str) -> str:
    bind = relay_url.removeprefix('http://').removeprefix('https://')
    return (
        f'External c3 relay is not reachable at {relay_url}. '
        f'Start it first with `c3 relay --bind {bind}` after running '
        '`python tools/dev/c3_tool.py --build --link` from the source checkout.'
    )


def _require_external_relay(relay_url: str) -> None:
    try:
        with _NO_PROXY_OPENER.open(f'{relay_url}/health', timeout=5) as resp:
            if resp.status >= 400:
                raise OSError(f'HTTP {resp.status}')
    except Exception as exc:
        raise SystemExit(_relay_help(relay_url)) from exc


def _post_json(url: str, payload: dict[str, object], timeout: float = 5.0) -> int:
    data = json.dumps(payload).encode('utf-8')
    req = urllib.request.Request(
        url,
        data=data,
        method='POST',
        headers={'Content-Type': 'application/json'},
    )
    with _NO_PROXY_OPENER.open(req, timeout=timeout) as resp:
        if resp.status >= 400:
            raise OSError(f'HTTP {resp.status}')
        return resp.status


def _register_relay_upstream(
    relay_url: str,
    name: str,
    address: str,
    server_id: str,
    server_instance_id: str,
    expected: CRMContract,
    max_payload_size: int,
) -> None:
    _post_json(
        f'{relay_url}/_register',
        {
            'name': name,
            'server_id': server_id,
            'server_instance_id': server_instance_id,
            'address': address,
            'crm_ns': expected.crm_ns,
            'crm_name': expected.crm_name,
            'crm_ver': expected.crm_ver,
            'abi_hash': expected.abi_hash,
            'signature_hash': expected.signature_hash,
            'max_payload_size': max_payload_size,
        },
    )


def _unregister_relay_upstream(relay_url: str, name: str, server_id: str) -> None:
    _post_json(f'{relay_url}/_unregister', {'name': name, 'server_id': server_id})


def _best_effort_unregister_relay_upstream(
    relay_url: str,
    name: str,
    server_id: str,
) -> None:
    try:
        _unregister_relay_upstream(relay_url, name, server_id)
    except urllib.error.HTTPError as exc:
        if exc.code != 404:
            print(f'Warning: failed to unregister relay route {name!r}: {exc}', file=sys.stderr)
    except Exception as exc:
        print(f'Warning: failed to unregister relay route {name!r}: {exc}', file=sys.stderr)


def _should_run_relay(_payload_size: int) -> bool:
    return True


def _best_effort_unregister_local(name: str) -> None:
    try:
        cc.unregister(name)
    except Exception as exc:
        print(f'Warning: failed to unregister local route {name!r}: {exc}', file=sys.stderr)


def _server_instance_id_for(name: str, address: str, crm_class: type) -> str:
    expected = crm_contract(crm_class)
    registry = _ProcessRegistry.get()
    client = registry._runtime_session.acquire_ipc_client(  # noqa: SLF001
        address,
        name,
        *expected.native_args(),
    )
    try:
        instance_id = client.server_instance_id
        if not isinstance(instance_id, str) or not instance_id:
            raise RuntimeError('registered server did not expose server_instance_id')
        return instance_id
    finally:
        registry._runtime_session.release_ipc_client(address)  # noqa: SLF001


def _cleanup():
    """Clean up stale IPC sockets and SHM."""
    try:
        from c_two.mem import cleanup_stale_shm
        cleanup_stale_shm()
    except Exception:
        pass


def _wait_sock(address: str, timeout: float = 5.0):
    from c_two.transport.client.util import ping
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if ping(address, timeout=0.05):
            return True
        time.sleep(0.05)
    return False


def _measure(proxy, payload, rounds: int, validate_len: bool = True) -> float:
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

    if validate_len:
        assert len(result) == len(payload), f'size mismatch: {len(result)} != {len(payload)}'
    return statistics.median(latencies) * 1000


# ---------------------------------------------------------------------------
# Thread-local mode
# ---------------------------------------------------------------------------

def bench_thread(payload_size: int) -> float:
    _ProcessRegistry.reset()
    cc.register(Echo, EchoImpl(), name='echo_thread')
    try:
        crm = cc.connect(Echo, name='echo_thread')
        payload = b'\xAB' * payload_size
        result_ms = _measure(crm, payload, _rounds(payload_size))
        cc.close(crm)
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

    ipc_overrides = {
        'pool_segment_size': _SEGMENT_SIZE,
        'max_pool_segments': _MAX_SEGMENTS,
    }
    cc.set_server(ipc_overrides=ipc_overrides)
    cc.set_client(ipc_overrides=ipc_overrides)
    cc.register(Echo, EchoImpl(), name='echo_ipc')
    address = cc.server_address()
    _wait_sock(address)

    payload = b'\xAB' * payload_size
    try:
        crm = cc.connect(Echo, name='echo_ipc', address=address)
        result_ms = _measure(crm, payload, _rounds(payload_size))
        cc.close(crm)
    finally:
        cc.unregister('echo_ipc')
        cc.shutdown()
    return result_ms


# ---------------------------------------------------------------------------
# Relay (HTTP) mode
# ---------------------------------------------------------------------------

def bench_relay(payload_size: int) -> float | None:
    global _ipc_counter
    _ipc_counter += 1
    _ProcessRegistry.reset()
    relay_url = f'http://127.0.0.1:{_relay_port}'
    _require_external_relay(relay_url)
    route_name = f'echo_relay_{os.getpid()}_{_ipc_counter}'
    local_registered = False
    route_registered = False
    server_id: str | None = None

    try:
        ipc_overrides = {
            'pool_segment_size': _SEGMENT_SIZE,
            'max_pool_segments': _MAX_SEGMENTS,
        }
        server_config = _resolve_server_ipc_config(ipc_overrides)
        max_payload_size = int(server_config['max_payload_size'])
        cc.set_server(ipc_overrides=ipc_overrides)
        cc.set_client(ipc_overrides=ipc_overrides)
        cc.register(Echo, EchoImpl(), name=route_name)
        local_registered = True
        address = cc.server_address()
        server_id = cc.server_id()
        if server_id is None:
            raise RuntimeError('registered server did not expose server_id')
        _wait_sock(address)
        server_instance_id = _server_instance_id_for(route_name, address, Echo)
        expected = crm_contract(Echo)

        _register_relay_upstream(
            relay_url,
            route_name,
            address,
            server_id,
            server_instance_id,
            expected,
            max_payload_size,
        )
        route_registered = True

        payload = b'\xAB' * payload_size
        try:
            crm = cc.connect(Echo, name=route_name, address=relay_url)
            result_ms = _measure(crm, payload, _rounds(payload_size))
            cc.close(crm)
            return result_ms
        except Exception as exc:
            print(f'  [relay FAILED: {exc}]', file=sys.stderr)
            return None
    finally:
        if route_registered and server_id is not None:
            _best_effort_unregister_relay_upstream(relay_url, route_name, server_id)
        if local_registered:
            _best_effort_unregister_local(route_name)
        cc.shutdown()


# ---------------------------------------------------------------------------
# IPC mode — dict payload (pickle path)
# ---------------------------------------------------------------------------

# dict sizes were previously capped at 100MB due to dedicated segment bugs.
# Now that ChunkRegistry + dedicated segments work correctly, test all sizes.
_DICT_MAX_SIZE = 2 * 1024 * 1024 * 1024  # 2GB — no practical limit

def _make_dict_payload(size: int) -> dict:
    """Create a dict whose pickled size is approximately `size` bytes."""
    return {'payload': b'\xAB' * size}


def bench_ipc_dict(payload_size: int) -> float | None:
    if payload_size > _DICT_MAX_SIZE:
        return None
    global _ipc_counter
    _ipc_counter += 1
    _ProcessRegistry.reset()

    ipc_overrides = {
        'pool_segment_size': _SEGMENT_SIZE,
        'max_pool_segments': _MAX_SEGMENTS,
    }
    cc.set_server(ipc_overrides=ipc_overrides)
    cc.set_client(ipc_overrides=ipc_overrides)
    cc.register(DictEcho, DictEchoImpl(), name='echo_dict')
    address = cc.server_address()
    _wait_sock(address)

    payload = _make_dict_payload(payload_size)
    try:
        crm = cc.connect(DictEcho, name='echo_dict', address=address)
        result_ms = _measure(crm, payload, _rounds(payload_size), validate_len=False)
        cc.close(crm)
    finally:
        cc.unregister('echo_dict')
        cc.shutdown()
    return result_ms


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------

def _human_size(n: int) -> str:
    if n >= 1024 * 1024 * 1024 and n % (1024 * 1024 * 1024) == 0:
        return f'{n // (1024 * 1024 * 1024)}GB'
    if n >= 1024 * 1024 and n % (1024 * 1024) == 0:
        return f'{n // (1024 * 1024)}MB'
    return f'{n}B'


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
    global _SEGMENT_SIZE, _MAX_SEGMENTS

    parser = argparse.ArgumentParser(description='Three-mode benchmark')
    parser.add_argument('--segment-size', type=int, default=2 * 1024 * 1024 * 1024,
                        help='Pool segment size in bytes (default: 2GB)')
    parser.add_argument('--max-segments', type=int, default=8,
                        help='Max pool segments (default: 8)')
    parser.add_argument('--output', type=str, default=None,
                        help='Output TSV path (default: auto-named by segment size)')
    args = parser.parse_args()
    _SEGMENT_SIZE = args.segment_size
    _MAX_SEGMENTS = args.max_segments

    seg_label = _human_size(_SEGMENT_SIZE)

    _cleanup()

    print('=' * 120)
    print('Three-Mode Benchmark: Thread-local vs IPC vs Relay (HTTP)')
    print(f'Payload types: bytes (identity fast path) | dict (pickle serde)')
    print(f'Warmup: {WARMUP}  |  Adaptive rounds (100/20/5)  |  Segment: {seg_label}×{_MAX_SEGMENTS}')
    print(
        'Relay path: '
        f'{RELAY_TRANSPORT_LABEL} '
        f'(client_request_http={RELAY_CLIENT_REQUEST_BODY_PATH}, '
        f'relay_request_to_ipc={RELAY_RELAY_REQUEST_BODY_PATH}, '
        f'response_http={RELAY_RESPONSE_BODY_PATH})'
    )
    print(f'Python: {sys.version}')
    print('=' * 120)
    header = (f'{"Size":>8s}  {"Rounds":>6s}  {"Thread (ms)":>12s}  '
              f'{"IPC-bytes":>12s}  {"IPC-dict":>12s}  {"dict/bytes":>11s}  '
              f'{"Relay (ms)":>12s}  {"IPC/Thd":>8s}')
    print(header)
    print('-' * 120)

    results: list[dict] = []

    for size_bytes, label in SIZES:
        rounds = _rounds(size_bytes)

        t_ms = bench_thread(size_bytes)
        i_ms = bench_ipc(size_bytes)
        d_ms = bench_ipc_dict(size_bytes)
        r_ms = bench_relay(size_bytes) if _should_run_relay(size_bytes) else None
        relay_path = RELAY_TRANSPORT_LABEL if r_ms is not None else 'N/A'

        ipc_ratio = f'{i_ms / t_ms:.1f}×' if t_ms > 0 else '—'
        dict_ratio = f'{d_ms / i_ms:.1f}×' if (d_ms is not None and i_ms > 0) else '—'

        print(f'{label:>8s}  {rounds:>6d}  {_fmt(t_ms):>12s}  '
              f'{_fmt(i_ms):>12s}  {_fmt(d_ms):>12s}  {dict_ratio:>11s}  '
              f'{_fmt(r_ms):>12s}  {ipc_ratio:>8s}')

        results.append({
            'size': label, 'size_bytes': size_bytes, 'rounds': rounds,
            'thread_ms': t_ms, 'ipc_ms': i_ms, 'ipc_dict_ms': d_ms, 'relay_ms': r_ms,
            'relay_path': relay_path,
        })

    # Summary
    t_vals = [r['thread_ms'] for r in results]
    i_vals = [r['ipc_ms'] for r in results]
    d_vals = [r['ipc_dict_ms'] for r in results if r['ipc_dict_ms'] is not None]
    r_vals = [r['relay_ms'] for r in results if r['relay_ms'] is not None]

    print('-' * 120)
    print(f'{"GeoMean":>8s}  {"":>6s}  {_fmt(_geomean(t_vals)):>12s}  '
          f'{_fmt(_geomean(i_vals)):>12s}  {_fmt(_geomean(d_vals)):>12s}  {"":>11s}  '
          f'{_fmt(_geomean(r_vals)):>12s}')
    print('=' * 120)

    # Write TSV
    if args.output:
        tsv_path = args.output
    else:
        results_dir = os.path.join(os.path.dirname(__file__), 'results')
        os.makedirs(results_dir, exist_ok=True)
        tsv_path = os.path.join(results_dir, f'benchmark_{seg_label.lower()}.tsv')
    with open(tsv_path, 'w') as f:
        f.write(
            'size\tsize_bytes\trounds\tthread_ms\tipc_bytes_ms\tipc_dict_ms\t'
            'relay_ms\trelay_path\trelay_client_request_http\t'
            'relay_request_to_ipc\trelay_response_http\n'
        )
        for r in results:
            dict_str = f'{r["ipc_dict_ms"]:.4f}' if r['ipc_dict_ms'] is not None else 'N/A'
            relay_str = f'{r["relay_ms"]:.4f}' if r['relay_ms'] is not None else 'N/A'
            f.write(
                f'{r["size"]}\t{r["size_bytes"]}\t{r["rounds"]}\t'
                f'{r["thread_ms"]:.4f}\t{r["ipc_ms"]:.4f}\t{dict_str}\t{relay_str}\t'
                f'{r["relay_path"]}\t{RELAY_CLIENT_REQUEST_BODY_PATH}\t'
                f'{RELAY_RELAY_REQUEST_BODY_PATH}\t{RELAY_RESPONSE_BODY_PATH}\n'
            )
    print(f'\nResults written to {os.path.abspath(tsv_path)}')

    _cleanup()


if __name__ == '__main__':
    main()
