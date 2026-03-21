"""
IPC v2 Cost Breakdown Benchmark
================================
Measures each component of the IPC v2 data path independently to attribute
latency to specific operations: memcpy, SHM lifecycle, serialization,
Event encoding, bytes concat, UDS control plane, thread dispatch, etc.

Usage:
    uv run python benchmarks/ipc_cost_breakdown.py
    uv run python benchmarks/ipc_cost_breakdown.py --sizes 1MB,100MB,1GB
    uv run python benchmarks/ipc_cost_breakdown.py --sizes 1MB --rounds 20
"""

import argparse
import gc
import os
import socket as _socket
import statistics
import struct
import sys
import tempfile
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from multiprocessing import shared_memory

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src')))

from c_two.rpc.event import Event, EventTag
from c_two.rpc.util.encoding import add_length_prefix, parse_message
from c_two.rpc.ipc.ipc_server import (
    _scatter_write_event_to_shm,
    _scatter_write_event_multi_to_shm,
    _read_and_release_shm,
    _shm_name,
    _encode_frame,
    _decode_frame,
)
from c_two.rpc.ipc.ipc_client import _send_frame_sync, _recv_frame_sync

# Also import the full RPC stack for end-to-end comparison
import c_two as cc
from c_two.rpc.server import _start


# ---------------------------------------------------------------------------
# Size helpers (reused from memory_benchmark)
# ---------------------------------------------------------------------------

_SIZE_UNITS = {
    'B': 1, 'KB': 1024, 'MB': 1024**2, 'GB': 1024**3,
    'K': 1024, 'M': 1024**2, 'G': 1024**3,
}


def parse_size(s: str) -> int:
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


def fmt_ms(ms: float) -> str:
    if ms < 0.001:
        return f'{ms * 1000:.2f}µs'
    elif ms < 1.0:
        return f'{ms:.3f}ms'
    elif ms < 1000:
        return f'{ms:.1f}ms'
    else:
        return f'{ms / 1000:.2f}s'


# ---------------------------------------------------------------------------
# Payload generation
# ---------------------------------------------------------------------------

def make_payload(size: int) -> bytes:
    if size <= 1024**2:
        return os.urandom(size)
    chunk = os.urandom(min(size, 1024 * 1024))
    repeats = size // len(chunk)
    remainder = size % len(chunk)
    return chunk * repeats + chunk[:remainder]


# ---------------------------------------------------------------------------
# Benchmark runner
# ---------------------------------------------------------------------------

def bench(func, rounds: int, warmup: int = 1) -> list[float]:
    """Run func() `rounds` times, return list of elapsed times in ms.
    
    func() should return None or a value (discarded). GC is disabled during
    measurement to avoid noise.
    """
    for _ in range(warmup):
        func()

    gc.disable()
    try:
        times = []
        for _ in range(rounds):
            t0 = time.perf_counter()
            func()
            t1 = time.perf_counter()
            times.append((t1 - t0) * 1000)
        return times
    finally:
        gc.enable()


def summarize(times: list[float]) -> dict:
    return {
        'median': statistics.median(times),
        'mean': statistics.mean(times),
        'min': min(times),
        'max': max(times),
        'stdev': statistics.stdev(times) if len(times) > 1 else 0.0,
    }


# ---------------------------------------------------------------------------
# Individual component benchmarks
# ---------------------------------------------------------------------------

def bench_memcpy(data: bytes, rounds: int) -> dict:
    """Pure Python memcpy: allocate bytearray + copy data into it."""
    size = len(data)
    def run():
        buf = bytearray(size)
        buf[:] = data
    return summarize(bench(run, rounds))


def bench_shm_lifecycle(data: bytes, rounds: int) -> dict:
    """Full SHM lifecycle: create + write + read + close + unlink."""
    size = len(data)
    counter = [0]
    def run():
        counter[0] += 1
        name = f'bd_{counter[0]:08x}'
        shm = shared_memory.SharedMemory(name=name, create=True, size=size)
        shm.buf[:size] = data
        result = bytes(shm.buf[:size])
        shm.close()
        shm.unlink()
        return result
    return summarize(bench(run, rounds))


def bench_shm_write(data: bytes, rounds: int) -> dict:
    """SHM write only: pre-create SHM, measure write time."""
    size = len(data)
    shm = shared_memory.SharedMemory(name='bd_write_bench', create=True, size=size)
    try:
        def run():
            shm.buf[:size] = data
        result = summarize(bench(run, rounds))
    finally:
        shm.close()
        shm.unlink()
    return result


def bench_shm_read(data: bytes, rounds: int) -> dict:
    """SHM read (bytes copy): pre-create SHM with data, measure read time."""
    size = len(data)
    shm = shared_memory.SharedMemory(name='bd_read_bench', create=True, size=size)
    shm.buf[:size] = data
    try:
        def run():
            return bytes(shm.buf[:size])
        result = summarize(bench(run, rounds))
    finally:
        shm.close()
        shm.unlink()
    return result


def bench_shm_create_destroy(size: int, rounds: int) -> dict:
    """SHM create + close + unlink only (no data transfer)."""
    counter = [0]
    def run():
        counter[0] += 1
        name = f'bd_cd_{counter[0]:08x}'
        shm = shared_memory.SharedMemory(name=name, create=True, size=max(size, 1))
        shm.close()
        shm.unlink()
    return summarize(bench(run, rounds))


def bench_bytes_concat(data: bytes, rounds: int) -> dict:
    """Bytes concatenation: add_length_prefix(err) + add_length_prefix(data)."""
    err = b'\x00'  # 1 byte error field (typical)
    def run():
        return add_length_prefix(err) + add_length_prefix(data)
    return summarize(bench(run, rounds))


def bench_event_roundtrip(data: bytes, rounds: int) -> dict:
    """Event.serialize() + Event.deserialize() round-trip."""
    event = Event(tag=EventTag.CRM_CALL, data=data)
    serialized = event.serialize()
    def run():
        s = event.serialize()
        Event.deserialize(s)
    return summarize(bench(run, rounds))


def bench_event_serialize_only(data: bytes, rounds: int) -> dict:
    """Event.serialize() only."""
    event = Event(tag=EventTag.CRM_CALL, data=data)
    def run():
        return event.serialize()
    return summarize(bench(run, rounds))


def bench_event_deserialize_only(data: bytes, rounds: int) -> dict:
    """Event.deserialize() only."""
    event = Event(tag=EventTag.CRM_CALL, data=data)
    serialized = event.serialize()
    def run():
        return Event.deserialize(serialized)
    return summarize(bench(run, rounds))


def bench_scatter_write(data: bytes, rounds: int) -> dict:
    """Scatter-write Event to SHM (the actual IPC v2 write path)."""
    method = b'echo'
    counter = [0]
    def run():
        counter[0] += 1
        name = f'bd_sw_{counter[0]:08x}'
        shm, written = _scatter_write_event_multi_to_shm(
            name, EventTag.CRM_CALL, [method, data]
        )
        shm.close()
        shm.unlink()
    return summarize(bench(run, rounds))


def bench_scatter_write_then_read(data: bytes, rounds: int) -> dict:
    """Scatter-write to SHM + bytes read back (one-way SHM path)."""
    method = b'echo'
    counter = [0]
    def run():
        counter[0] += 1
        name = f'bd_sr_{counter[0]:08x}'
        shm, written = _scatter_write_event_multi_to_shm(
            name, EventTag.CRM_CALL, [method, data]
        )
        shm.close()
        result = _read_and_release_shm(name, written)
        return result
    return summarize(bench(run, rounds))


def bench_uds_roundtrip(rounds: int) -> dict:
    """UDS control plane round-trip: send tiny frame + recv response.
    
    Sets up a minimal echo server on a Unix socket.
    """
    sock_path = os.path.join(tempfile.gettempdir(), f'bd_uds_{os.getpid()}.sock')
    try:
        os.unlink(sock_path)
    except FileNotFoundError:
        pass

    ready = threading.Event()
    stop = threading.Event()

    def server_func():
        srv = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        srv.setsockopt(_socket.SOL_SOCKET, _socket.SO_REUSEADDR, 1)
        srv.bind(sock_path)
        srv.listen(1)
        srv.settimeout(0.5)
        ready.set()
        conn = None
        try:
            conn, _ = srv.accept()
            while not stop.is_set():
                try:
                    header = b''
                    while len(header) < 4:
                        chunk = conn.recv(4 - len(header))
                        if not chunk:
                            return
                        header += chunk
                    total_len = struct.unpack('<I', header)[0]
                    body = b''
                    while len(body) < total_len:
                        chunk = conn.recv(total_len - len(body))
                        if not chunk:
                            return
                        body += chunk
                    # Echo back the same frame
                    conn.sendall(header + body)
                except _socket.timeout:
                    continue
        finally:
            if conn:
                conn.close()
            srv.close()

    t = threading.Thread(target=server_func, daemon=True)
    t.start()
    ready.wait(5)

    # Client
    sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
    sock.connect(sock_path)

    # Small payload: 30 bytes (typical SHM reference)
    small_payload = b'x' * 30
    rid = 'test-rid'
    frame = _encode_frame(rid, 0, small_payload)

    def run():
        _send_frame_sync(sock, frame)
        raw = _recv_frame_sync(sock)
        return raw

    try:
        result = summarize(bench(run, rounds))
    finally:
        stop.set()
        sock.close()
        t.join(timeout=3)
        try:
            os.unlink(sock_path)
        except FileNotFoundError:
            pass
    return result


def bench_thread_dispatch(rounds: int) -> dict:
    """Thread pool submit + future.result() overhead (no-op function)."""
    pool = ThreadPoolExecutor(max_workers=4)
    def noop():
        return None
    def run():
        fut = pool.submit(noop)
        fut.result()
    try:
        result = summarize(bench(run, rounds))
    finally:
        pool.shutdown(wait=False)
    return result


# ---------------------------------------------------------------------------
# Full IPC v2 end-to-end benchmark (reuses c-two RPC stack)
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


@cc.icrm(namespace='bench.diag', version='0.1.0')
class IBench:
    def echo(self, data: bytes) -> bytes:
        ...


class BenchCRM:
    def echo(self, data: bytes) -> bytes:
        return data


_bench_counter = [0]


def _start_bench_server(protocol: str) -> tuple:
    """Start a BenchCRM server, return (address, server)."""
    _bench_counter[0] += 1
    idx = _bench_counter[0]
    addr = f'{protocol}://diag-bench-{idx}'
    config = cc.rpc.ServerConfig(
        name=f'DiagBench_{protocol}_{idx}',
        crm=BenchCRM(),
        icrm=IBench,
        bind_address=addr,
    )
    server = cc.rpc.Server(config)
    _start(server._state)

    for _ in range(100):
        try:
            if cc.rpc.Client.ping(addr, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.05)

    return addr, server


def _shutdown_bench_server(addr: str, server) -> None:
    try:
        cc.rpc.Client.shutdown(addr, timeout=5.0)
    except Exception:
        pass
    time.sleep(0.2)
    try:
        server.stop()
    except Exception:
        pass


def bench_full_ipc_v2(data: bytes, rounds: int) -> dict:
    """Full IPC v2 echo round-trip through the C-Two RPC stack."""
    addr, server = _start_bench_server('ipc-v2')

    client = cc.rpc.Client(addr)
    # Warmup
    client.call('echo', data[:min(1024, len(data))])

    gc.disable()
    try:
        times = []
        for _ in range(rounds):
            t0 = time.perf_counter()
            result = client.call('echo', data)
            t1 = time.perf_counter()
            times.append((t1 - t0) * 1000)
    finally:
        gc.enable()
        client.terminate()
        _shutdown_bench_server(addr, server)

    return summarize(times)


def bench_full_thread(data: bytes, rounds: int) -> dict:
    """Full thread:// echo round-trip (with serialization, NOT direct call).
    
    Note: thread:// with direct call enabled passes Python objects by reference
    (O(1)). This benchmark uses the Client.call() path which goes through the
    standard serialization pipeline, to provide a fair comparison with IPC.
    """
    addr, server = _start_bench_server('thread')

    client = cc.rpc.Client(addr)
    # Warmup
    client.call('echo', data[:min(1024, len(data))])

    gc.disable()
    try:
        times = []
        for _ in range(rounds):
            t0 = time.perf_counter()
            result = client.call('echo', data)
            t1 = time.perf_counter()
            times.append((t1 - t0) * 1000)
    finally:
        gc.enable()
        client.terminate()
        _shutdown_bench_server(addr, server)

    return summarize(times)


# ---------------------------------------------------------------------------
# Main driver
# ---------------------------------------------------------------------------

def auto_rounds(size: int, base: int) -> int:
    if size <= 1024**2:
        return base
    elif size <= 10 * 1024**2:
        return max(base // 5, 5)
    elif size <= 100 * 1024**2:
        return max(base // 10, 3)
    elif size <= 512 * 1024**2:
        return max(base // 20, 3)
    else:
        return max(base // 50, 3)


def run_all(sizes: list[int], base_rounds: int) -> None:
    # Components to benchmark (name, func_factory)
    # func_factory(data, rounds) -> dict  OR  func_factory(rounds) -> dict
    components = [
        ('1. Pure memcpy (×1)',            lambda d, r: bench_memcpy(d, r)),
        ('2. SHM create+destroy (no I/O)', lambda d, r: bench_shm_create_destroy(len(d), r)),
        ('3. SHM write only',              lambda d, r: bench_shm_write(d, r)),
        ('4. SHM read only (bytes copy)',  lambda d, r: bench_shm_read(d, r)),
        ('5. SHM full lifecycle',          lambda d, r: bench_shm_lifecycle(d, r)),
        ('6. scatter-write → SHM',         lambda d, r: bench_scatter_write(d, r)),
        ('7. scatter-write + read back',   lambda d, r: bench_scatter_write_then_read(d, r)),
        ('8. bytes concat (1B + payload)', lambda d, r: bench_bytes_concat(d, r)),
        ('9. Event.serialize()',           lambda d, r: bench_event_serialize_only(d, r)),
        ('10. Event.deserialize()',        lambda d, r: bench_event_deserialize_only(d, r)),
        ('11. Event round-trip',           lambda d, r: bench_event_roundtrip(d, r)),
        ('12. UDS round-trip (30B)',       None),  # special: no data dependency
        ('13. ThreadPool dispatch',        None),  # special: no data dependency
        ('14. Full thread:// (serialized)',lambda d, r: bench_full_thread(d, r)),
        ('15. Full IPC v2 echo',           lambda d, r: bench_full_ipc_v2(d, r)),
    ]

    # Pre-run fixed-cost benchmarks
    print('Measuring fixed-cost components...')
    uds_result = bench_uds_roundtrip(max(base_rounds, 100))
    dispatch_result = bench_thread_dispatch(max(base_rounds, 100))
    print(f'  UDS round-trip (30B):   {fmt_ms(uds_result["median"])} median')
    print(f'  ThreadPool dispatch:    {fmt_ms(dispatch_result["median"])} median')
    print()

    # Results table: results[component_name][size] = summary_dict
    results: dict[str, dict[int, dict]] = {}

    for size in sizes:
        rounds = auto_rounds(size, base_rounds)
        print(f'=== {human_size(size)} ({rounds} rounds) ===')

        # Generate payload
        print(f'  Generating {human_size(size)} payload...', end='', flush=True)
        data = make_payload(size)
        print(' done')

        for name, factory in components:
            if name == '12. UDS round-trip (30B)':
                results.setdefault(name, {})[size] = uds_result
                continue
            if name == '13. ThreadPool dispatch':
                results.setdefault(name, {})[size] = dispatch_result
                continue

            print(f'  {name}...', end='', flush=True)
            try:
                summary = factory(data, rounds)
                results.setdefault(name, {})[size] = summary
                print(f' {fmt_ms(summary["median"])}')
            except Exception as e:
                print(f' ERROR: {e}')
                results.setdefault(name, {})[size] = {'median': float('nan'), 'error': str(e)}

        # Free payload between sizes to avoid OOM
        del data
        gc.collect()
        print()

    # Print summary table
    print_summary_table(sizes, results, components)
    print_cost_attribution(sizes, results)


def print_summary_table(sizes: list[int], results: dict, components: list) -> None:
    print()
    print('=' * 100)
    print('IPC v2 Cost Breakdown — Median Latency per Component')
    print('=' * 100)

    # Header
    col_w = 12
    name_w = 38
    header = f'{"Component":<{name_w}}'
    for s in sizes:
        header += f' | {human_size(s):>{col_w}}'
    print(header)
    print('-' * name_w + ('-+-' + '-' * col_w) * len(sizes))

    for name, _ in components:
        row = f'{name:<{name_w}}'
        for s in sizes:
            val = results.get(name, {}).get(s, {})
            med = val.get('median', float('nan'))
            if med != med:  # nan check
                cell = 'ERROR'
            else:
                cell = fmt_ms(med)
            row += f' | {cell:>{col_w}}'
        print(row)

    print()


def print_cost_attribution(sizes: list[int], results: dict) -> None:
    print()
    print('=' * 100)
    print('Cost Attribution Analysis')
    print('=' * 100)

    for size in sizes:
        memcpy1 = results.get('1. Pure memcpy (×1)', {}).get(size, {}).get('median', 0)
        shm_cd = results.get('2. SHM create+destroy (no I/O)', {}).get(size, {}).get('median', 0)
        shm_w = results.get('3. SHM write only', {}).get(size, {}).get('median', 0)
        shm_r = results.get('4. SHM read only (bytes copy)', {}).get(size, {}).get('median', 0)
        shm_full = results.get('5. SHM full lifecycle', {}).get(size, {}).get('median', 0)
        scatter_w = results.get('6. scatter-write → SHM', {}).get(size, {}).get('median', 0)
        scatter_wr = results.get('7. scatter-write + read back', {}).get(size, {}).get('median', 0)
        concat = results.get('8. bytes concat (1B + payload)', {}).get(size, {}).get('median', 0)
        evt_ser = results.get('9. Event.serialize()', {}).get(size, {}).get('median', 0)
        evt_deser = results.get('10. Event.deserialize()', {}).get(size, {}).get('median', 0)
        uds = results.get('12. UDS round-trip (30B)', {}).get(size, {}).get('median', 0)
        dispatch = results.get('13. ThreadPool dispatch', {}).get(size, {}).get('median', 0)
        full_thread = results.get('14. Full thread:// (serialized)', {}).get(size, {}).get('median', 0)
        full_ipc = results.get('15. Full IPC v2 echo', {}).get(size, {}).get('median', 0)

        print(f'\n--- {human_size(size)} ---')
        print()

        # IPC v2 copy chain decomposition:
        # Request: scatter-write(args) → SHM read → deserialize(bytes())
        # Response: concat → scatter-write(result) → SHM read → deserialize(bytes())
        #
        # Estimated copies: 
        #   2× scatter-write to SHM (= 2× shm_w approx, plus create overhead)
        #   2× SHM read bytes() (= 2× shm_r)
        #   1× concat (response)
        #   1× deserialize bytes() (≈ memcpy, counted in SHM read)
        #
        # But some of these overlap. Better to use the measured components:

        # Bottom-up estimate for full IPC v2:
        # = 2 × (scatter_write_to_shm + SHM_read) + 1 × concat + 2 × UDS + dispatch + misc
        data_copies = 2 * scatter_wr  # 2 one-way SHM trips (request + response)
        response_concat = concat       # 1 response concat
        control_plane = 2 * uds        # 2 UDS messages (request ref + response ref)
        thread_overhead = dispatch
        protocol_misc = 0.1            # Event header parsing, method lookup, etc.

        estimated_total = data_copies + response_concat + control_plane + thread_overhead + protocol_misc

        print(f'  Component breakdown (estimated):')
        print(f'    Data transfer (2× SHM write+read):  {fmt_ms(data_copies):>10}')
        print(f'    Response concat (1×):                {fmt_ms(response_concat):>10}')
        print(f'    UDS control plane (2×):              {fmt_ms(control_plane):>10}')
        print(f'    Thread dispatch (1×):                {fmt_ms(thread_overhead):>10}')
        print(f'    ----------------------------------------')
        print(f'    Estimated total:                     {fmt_ms(estimated_total):>10}')
        print(f'    Actual IPC v2:                       {fmt_ms(full_ipc):>10}')
        print(f'    Actual thread (serialized):          {fmt_ms(full_thread):>10}')

        if full_ipc > 0:
            ratio = estimated_total / full_ipc
            print(f'    Estimate accuracy:                   {ratio:.1%}')
            data_pct = (data_copies / full_ipc) * 100 if full_ipc > 0 else 0
            concat_pct = (response_concat / full_ipc) * 100 if full_ipc > 0 else 0
            uds_pct = (control_plane / full_ipc) * 100 if full_ipc > 0 else 0
            dispatch_pct = (thread_overhead / full_ipc) * 100 if full_ipc > 0 else 0
            print()
            print(f'    Cost attribution (% of actual):')
            print(f'      Data copies:     {data_pct:5.1f}%  ← {"FUNDAMENTAL" if data_pct > 60 else "significant"}')
            print(f'      Response concat: {concat_pct:5.1f}%  ← {"optimizable" if concat_pct > 5 else "minor"}')
            print(f'      UDS control:     {uds_pct:5.1f}%')
            print(f'      Thread dispatch: {dispatch_pct:5.1f}%')

        # Bandwidth analysis
        if memcpy1 > 0 and size >= 1024**2:
            bw_gbps = (size / 1024**3) / (memcpy1 / 1000)
            print(f'\n    Memory bandwidth:  {bw_gbps:.1f} GB/s (from pure memcpy)')
            total_data_moved = size * 6  # 6 copies in IPC v2
            theoretical_min = (total_data_moved / 1024**3) / bw_gbps * 1000
            print(f'    Theoretical min (6× {human_size(size)} @ {bw_gbps:.1f}GB/s): {fmt_ms(theoretical_min)}')
            if full_ipc > 0:
                efficiency = theoretical_min / full_ipc * 100
                print(f'    Bandwidth efficiency: {efficiency:.0f}%')


def main():
    parser = argparse.ArgumentParser(description='IPC v2 Cost Breakdown Benchmark')
    parser.add_argument(
        '--sizes', type=str,
        default='1MB,10MB,100MB,512MB,1GB',
        help='Comma-separated payload sizes (default: 1MB,10MB,100MB,512MB,1GB)'
    )
    parser.add_argument(
        '--rounds', type=int, default=10,
        help='Base rounds per measurement (auto-scaled for large payloads)'
    )
    args = parser.parse_args()

    sizes = [parse_size(s) for s in args.sizes.split(',')]
    print(f'IPC v2 Cost Breakdown Benchmark')
    print(f'Payload sizes: {", ".join(human_size(s) for s in sizes)}')
    print(f'Base rounds: {args.rounds}')
    print(f'Python: {sys.version}')
    print()

    run_all(sizes, args.rounds)


if __name__ == '__main__':
    main()
