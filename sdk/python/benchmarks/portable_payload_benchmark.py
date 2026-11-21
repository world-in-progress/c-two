"""Reproducible headless benchmark for explicit FastDB Payload, server streams,
multi-subscriber fan-out, and Ray / Ray Compiled Graphs comparison.

Uses only public C-Two, FastDB, NumPy, and (optional) Ray APIs under C-Two's
current single-owner copy-then-release semantics (ADR 0001).

Suites:
  - stages:   Single-process FastDB build / execute / binary_bytes / open_copy /
              open_external / view-reduce decomposition (256 KiB, 4 MiB, 32 MiB).
  - coupling: Bidirectional small-packet coupling exchange (4, 16, 64 KiB on
              direct IPC FastDB unary; 16 KiB paired with pickle-arrays,
              Ray detached named actor, and Ray Compiled Graphs).
  - field:    Small-request -> full-field grid snapshot & live stream
              (256 KiB, 1, 4, 16, 32 MiB on direct IPC FastDB unary & stream;
              4 MiB & 32 MiB paired with cc.hold, explicit 1-hop c3 relay stream,
              pickle-arrays, Ray named actor, and Ray Compiled Graphs; separates
              published-frame vs fresh-frame workloads).
  - fanout:   Multi-subscriber amplification baseline (R = 1, 2, 4 concurrent
              client processes subscribing via @cc.stream at 4 MiB and 32 MiB
              over direct IPC and 1-hop c3 relay).
  - pubsub_fanout: Matching payload sizes and reader counts via @cc.topic over
                   direct IPC, one relay, and two relays in separate IPC roots.
  - kostya:   Structured coordinate replacement baseline (1K, 100K, 1M rows with
              u32 row_id, three f64 columns, 50,000 rotating strings, and exact
              columnar sum across explicit FastDB, pickle-arrays, and Ray actor).

Run (C-Two only, base environment):
    C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync python \
        sdk/python/benchmarks/portable_payload_benchmark.py --suite stages --smoke

Run with Ray 2.56+ (benchmark dependency group, same Python interpreter):
    RAY_ENABLE_UV_RUN_RUNTIME_ENV=0 C2_RELAY_ANCHOR_ADDRESS= \
        .venv/bin/python sdk/python/benchmarks/portable_payload_benchmark.py \
        --suite all --include-ray --output /path/to/results.json
"""

from __future__ import annotations

import argparse
from collections.abc import Iterator
from contextlib import ExitStack
import gc
import importlib.metadata
import json
import math
import multiprocessing as mp
import os
from pathlib import Path
import platform
from queue import Empty
try:
    import resource
except ImportError:  # Windows
    resource = None  # type: ignore[assignment]
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
from threading import BrokenBarrierError
import time
from typing import Any
import urllib.request

import numpy as np

import c_two as cc
from fastdb4py.payload import (
    BuildPolicy,
    Builder,
    CompiledSpec,
    ExternalBytes,
    Payload,
)

SCHEMA_VERSION = 'c-two.portable-payload-benchmark.v1'
KIB = 1024
MIB = 1024 * 1024
DEFAULT_ROLE_BUDGET_BYTES = 1024 * MIB  # 1 GiB role memory budget
RAY_OBJECT_STORE_BYTES = 1536 * MIB     # Safe below macOS 2 GiB Ray plasma cap
SIMULATED_STEP_MS = (5.0, 20.0, 50.0, 100.0)
_NO_PROXY_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))

# Single-direction full-payload copy lower bounds on SHM path (fastdb4py 0.2.4,
# excluding kernel/page-zeroing/metadata/initial array construction/client reduction).
COPY_LEDGER_SHM_UNARY = {
    'fastdb_image_to_python_bytes': 2,
    'python_bytes_to_ctwo_send_vec': 1,
    'ctwo_shm_write_and_receive_local_vec': 2,
    'python_bytes_view_ctypes_and_fastdb_open_copy': 3,
    'total_lower_bound_one_way': 8,
    'ctwo_transport_copies': 2,
    'fastdb_python_glue_copies': 6,
}

COPY_LEDGER_SHM_STREAM = {
    'fastdb_image_to_python_bytes': 2,
    'python_bytes_to_ctwo_send_vec': 0,  # stream writes PyBackedBytes directly
    'ctwo_shm_write_and_receive_local_vec': 2,
    'python_bytes_view_ctypes_and_fastdb_open_copy': 3,
    'total_lower_bound_one_way': 7,
    'ctwo_transport_copies': 2,
    'fastdb_python_glue_copies': 5,
}

# Portable fastdb.payload.v1 specs.
FIELD_SPEC: dict[str, Any] = {
    'schema': 'fastdb.payload.v1',
    'profile': 'record.v1',
    'components': [],
    'entries': [
        {'id': 'seq', 'cardinality': 'one', 'type': {'kind': 'u32', 'nullable': False}},
        {'id': 'rows', 'cardinality': 'one', 'type': {'kind': 'u32', 'nullable': False}},
        {'id': 'cols', 'cardinality': 'one', 'type': {'kind': 'u32', 'nullable': False}},
        {'id': 'values', 'cardinality': 'one', 'type': {'kind': 'bytes', 'nullable': False}},
    ],
}

KOSTYA_SPEC: dict[str, Any] = {
    'schema': 'fastdb.payload.v1',
    'profile': 'record.v1',
    'components': [],
    'entries': [
        {'id': 'count', 'cardinality': 'one', 'type': {'kind': 'u32', 'nullable': False}},
        {'id': 'row_id', 'cardinality': 'one', 'type': {'kind': 'bytes', 'nullable': False}},
        {'id': 'x', 'cardinality': 'one', 'type': {'kind': 'bytes', 'nullable': False}},
        {'id': 'y', 'cardinality': 'one', 'type': {'kind': 'bytes', 'nullable': False}},
        {'id': 'z', 'cardinality': 'one', 'type': {'kind': 'bytes', 'nullable': False}},
        {'id': 'name_index', 'cardinality': 'one', 'type': {'kind': 'bytes', 'nullable': False}},
        {'id': 'name_table', 'cardinality': 'many', 'type': {'kind': 'str', 'nullable': False}},
    ],
}

_FIELD_SPEC_BYTES = json.dumps(FIELD_SPEC, separators=(',', ':')).encode()
_KOSTYA_SPEC_BYTES = json.dumps(KOSTYA_SPEC, separators=(',', ':')).encode()
_COMPILED_CACHE: dict[str, CompiledSpec] = {}


def _compiled(key: str) -> CompiledSpec:
    spec = _COMPILED_CACHE.get(key)
    if spec is None:
        raw = _FIELD_SPEC_BYTES if key == 'field' else _KOSTYA_SPEC_BYTES
        spec = CompiledSpec.compile(raw)
        _COMPILED_CACHE[key] = spec
    return spec


# ---------------------------------------------------------------------------
# Deterministic payload builders & verifiers
# ---------------------------------------------------------------------------


def make_field_array(n_bytes: int) -> np.ndarray:
    if n_bytes % 8 != 0 or n_bytes < 8:
        raise ValueError(f'field size must be a positive multiple of 8 bytes, got {n_bytes}')
    count = n_bytes // 8
    return np.arange(count, dtype='<f8')


def expected_field_sum(count: int, seq: int) -> float:
    # Base sum(0..count-1) = count*(count-1)/2; element 0 replaced with seq + 0.5.
    return float(count * (count - 1) // 2) + (seq + 0.5)


def stamp_field_array(base: np.ndarray, seq: int) -> np.ndarray:
    arr = base.copy()
    arr[0] = seq + 0.5
    return arr


def build_field_payload(arr: np.ndarray, seq: int) -> tuple[Payload, dict[str, Any]]:
    spec = _compiled('field')
    raw = arr.tobytes()
    builder = Builder.create(spec)
    try:
        builder.entry_begin(0, 1).value_u32(seq)
        builder.entry_begin(1, 1).value_u32(1)
        builder.entry_begin(2, 1).value_u32(int(arr.size))
        builder.entry_begin(3, 1).value_bytes(raw)
        plan = builder.freeze()
    finally:
        builder.close()
    try:
        built = plan.execute(BuildPolicy.ALLOW_STAGING)
        report = {
            'execution_mode': str(built.report.mode),
            'staging_bytes': int(built.report.staging_bytes),
            'used_bytes': int(built.report.used_bytes),
        }
        return built.payload, report
    finally:
        plan.close()


def read_and_reduce_field_payload(payload: Payload) -> tuple[int, int, float, float]:
    """Return (seq, count, sum_value, extract_to_reduce_ms)."""
    t0 = time.perf_counter()
    with payload.entry_view(0) as v_seq:
        with v_seq.at(0) as c_seq:
            seq = int(c_seq.get_u32())
    with payload.entry_view(2) as v_cols:
        with v_cols.at(0) as c_cols:
            count = int(c_cols.get_u32())
    with payload.entry_view(3) as v_vals:
        with v_vals.at(0) as cell:
            with cell.acquire() as acc:
                arr = np.frombuffer(acc.bytes(), dtype='<f8')
                total = float(arr.sum())
    t1 = time.perf_counter()
    return seq, count, total, (t1 - t0) * 1e3


def verify_field_result(seq: int, expected_seq: int, count: int, expected_count: int, total: float) -> None:
    if seq != expected_seq:
        raise AssertionError(f'seq mismatch: expected {expected_seq}, got {seq}')
    if count != expected_count:
        raise AssertionError(f'count mismatch: expected {expected_count}, got {count}')
    exp = expected_field_sum(expected_count, expected_seq)
    if not math.isclose(total, exp, rel_tol=1e-12, abs_tol=1e-6):
        raise AssertionError(f'sum mismatch for seq={seq}: expected {exp}, got {total}')


def _kostya_expected_sum(n: int) -> float:
    return 0.6 * n * (n - 1) / 2.0


def build_kostya_numpy_dict(n: int) -> dict[str, Any]:
    idx = np.arange(n, dtype='<u4')
    return {
        'row_id': idx,
        'x': idx.astype('<f8') * 0.1,
        'y': idx.astype('<f8') * 0.2,
        'z': idx.astype('<f8') * 0.3,
        'name': [f'coord_{i % 50000:05d}' for i in range(n)],
    }


def build_kostya_fastdb_payload(n: int) -> tuple[Payload, dict[str, Any]]:
    spec = _compiled('kostya')
    idx = np.arange(n, dtype='<u4')
    x = (idx.astype('<f8') * 0.1).tobytes()
    y = (idx.astype('<f8') * 0.2).tobytes()
    z = (idx.astype('<f8') * 0.3).tobytes()
    unique_count = min(n, 50_000)
    name_index = (idx % unique_count).astype('<u2').tobytes()
    name_table = [f'coord_{i:05d}' for i in range(unique_count)]

    builder = Builder.create(spec)
    try:
        builder.entry_begin(0, 1).value_u32(n)
        builder.entry_begin(1, 1).value_bytes(idx.tobytes())
        builder.entry_begin(2, 1).value_bytes(x)
        builder.entry_begin(3, 1).value_bytes(y)
        builder.entry_begin(4, 1).value_bytes(z)
        builder.entry_begin(5, 1).value_bytes(name_index)
        builder.entry_begin(6, unique_count)
        for name in name_table:
            builder.value_str(name)
        plan = builder.freeze()
    finally:
        builder.close()
    try:
        built = plan.execute(BuildPolicy.ALLOW_STAGING)
        report = {
            'execution_mode': str(built.report.mode),
            'staging_bytes': int(built.report.staging_bytes),
            'used_bytes': int(built.report.used_bytes),
        }
        return built.payload, report
    finally:
        plan.close()


def reduce_kostya_fastdb(payload: Payload, n: int) -> tuple[float, float, float]:
    """Return (sum_value, extract_ms, reduce_ms)."""
    t0 = time.perf_counter()
    with payload.entry_view(0) as v_count:
        with v_count.at(0) as c_count:
            count = int(c_count.get_u32())
    assert count == n
    with payload.entry_view(2) as vx, payload.entry_view(3) as vy, payload.entry_view(4) as vz:
        with vx.at(0) as cx, vy.at(0) as cy, vz.at(0) as cz:
            with cx.acquire() as ax, cy.acquire() as ay, cz.acquire() as az:
                x_arr = np.frombuffer(ax.bytes(), dtype='<f8')
                y_arr = np.frombuffer(ay.bytes(), dtype='<f8')
                z_arr = np.frombuffer(az.bytes(), dtype='<f8')
                t1 = time.perf_counter()
                total = float(x_arr.sum() + y_arr.sum() + z_arr.sum())
                t2 = time.perf_counter()
    # Spot-check dictionary string table integrity (first and last row).
    with payload.entry_view(5) as v_idx, payload.entry_view(6) as v_tbl:
        with v_idx.at(0) as c_idx:
            with c_idx.acquire() as a_idx:
                idx_arr = np.frombuffer(a_idx.bytes(), dtype='<u2')
                first_i = int(idx_arr[0])
                last_i = int(idx_arr[n - 1])
        with v_tbl.at(first_i) as c0:
            with c0.acquire() as a0:
                assert a0.str() == 'coord_00000'
        with v_tbl.at(last_i) as c1:
            with c1.acquire() as a1:
                assert a1.str() == f'coord_{(n - 1) % 50000:05d}'
    exp = _kostya_expected_sum(n)
    if not math.isclose(total, exp, rel_tol=1e-9, abs_tol=1e-6):
        raise AssertionError(f'kostya sum mismatch: expected {exp}, got {total}')
    return total, (t1 - t0) * 1e3, (t2 - t1) * 1e3


def reduce_kostya_arrays(data: dict[str, Any], n: int) -> tuple[float, float, float]:
    t0 = time.perf_counter()
    x_arr = data['x']
    y_arr = data['y']
    z_arr = data['z']
    t1 = time.perf_counter()
    total = float(x_arr.sum() + y_arr.sum() + z_arr.sum())
    t2 = time.perf_counter()
    assert len(data['name']) == n
    assert data['name'][0] == 'coord_00000'
    assert data['name'][-1] == f'coord_{(n - 1) % 50000:05d}'
    exp = _kostya_expected_sum(n)
    if not math.isclose(total, exp, rel_tol=1e-9, abs_tol=1e-6):
        raise AssertionError(f'kostya sum mismatch: expected {exp}, got {total}')
    return total, (t1 - t0) * 1e3, (t2 - t1) * 1e3


# ---------------------------------------------------------------------------
# Statistics & environment metadata (strictly privacy-safe)
# ---------------------------------------------------------------------------


def nearest_rank(samples: list[float], pct: float) -> float:
    ordered = sorted(samples)
    if not ordered:
        return 0.0
    rank = max(1, math.ceil(pct * len(ordered)))
    return ordered[min(rank - 1, len(ordered) - 1)]


def summarize_ms(samples_ms: list[float], *, payload_bytes: int, bidirectional: bool = False) -> dict[str, Any]:
    p50 = nearest_rank(samples_ms, 0.50)
    p95 = nearest_rank(samples_ms, 0.95)
    p99 = nearest_rank(samples_ms, 0.99)
    mean = statistics.fmean(samples_ms)
    logical_moved = payload_bytes * (2 if bidirectional else 1)
    gib_s = (logical_moved / (p50 / 1000.0)) / (1024 ** 3) if p50 > 0 else 0.0
    fps = 1000.0 / p50 if p50 > 0 else 0.0
    return {
        'samples': len(samples_ms),
        'p50_ms': round(p50, 4),
        'p95_ms': round(p95, 4),
        'p99_ms': round(p99, 4),
        'mean_ms': round(mean, 4),
        'min_ms': round(min(samples_ms), 4),
        'max_ms': round(max(samples_ms), 4),
        'effective_gib_s_p50': round(gib_s, 4),
        'frames_per_s_p50': round(fps, 2),
        'step_overhead_share_p95': {
            f'{int(step)}ms': round(p95 / (step + p95), 4)
            for step in SIMULATED_STEP_MS
        },
    }


def _windows_process_memory_mib() -> tuple[float, float] | None:
    if os.name != 'nt':
        return None
    try:
        import ctypes
        from ctypes import wintypes

        class _ProcessMemoryCounters(ctypes.Structure):
            _fields_ = [
                ('cb', wintypes.DWORD),
                ('PageFaultCount', wintypes.DWORD),
                ('PeakWorkingSetSize', ctypes.c_size_t),
                ('WorkingSetSize', ctypes.c_size_t),
                ('QuotaPeakPagedPoolUsage', ctypes.c_size_t),
                ('QuotaPagedPoolUsage', ctypes.c_size_t),
                ('QuotaPeakNonPagedPoolUsage', ctypes.c_size_t),
                ('QuotaNonPagedPoolUsage', ctypes.c_size_t),
                ('PagefileUsage', ctypes.c_size_t),
                ('PeakPagefileUsage', ctypes.c_size_t),
            ]

        counters = _ProcessMemoryCounters()
        counters.cb = ctypes.sizeof(counters)
        handle = ctypes.windll.kernel32.GetCurrentProcess()
        if ctypes.windll.psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
            return (
                round(counters.PeakWorkingSetSize / MIB, 2),
                round(counters.WorkingSetSize / MIB, 2),
            )
    except Exception:
        return None
    return None


def peak_rss_mib() -> float:
    if resource is None:
        win = _windows_process_memory_mib()
        return win[0] if win is not None else 0.0
    return _ru_maxrss_mib(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss)


def _ru_maxrss_mib(usage: int) -> float:
    # macOS reports bytes; Linux reports KiB.
    if sys.platform == 'darwin':
        return round(usage / MIB, 2)
    return round(usage / KIB, 2)


def live_rss_mib() -> float | None:
    """Best-effort instantaneous resident set size of the current process in MiB."""
    if os.name == 'nt':
        win = _windows_process_memory_mib()
        return win[1] if win is not None else None
    try:
        out = subprocess.check_output(
            ['ps', '-o', 'rss=', '-p', str(os.getpid())],
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=2,
        ).strip()
        return round(int(out) / KIB, 2) if out else None
    except Exception:
        return None


def _pkg_version(name: str) -> str | None:
    try:
        return importlib.metadata.version(name)
    except importlib.metadata.PackageNotFoundError:
        return None


def _git_short_head() -> str | None:
    try:
        out = subprocess.check_output(
            ['git', 'rev-parse', '--short', 'HEAD'],
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=5,
        ).strip()
        return out or None
    except Exception:
        return None


def _c3_version(c3_bin: str | None) -> str | None:
    if not c3_bin:
        return None
    try:
        return subprocess.check_output([c3_bin, '--version'], text=True, timeout=5).strip()
    except Exception:
        return None


def collect_environment(*, c3_bin: str | None, include_ray: bool) -> dict[str, Any]:
    gil_enabled = getattr(sys, '_is_gil_enabled', lambda: True)()
    env: dict[str, Any] = {
        'os': platform.system(),
        'os_release': platform.release(),
        'arch': platform.machine(),
        'logical_cpus': os.cpu_count(),
        'python_version': platform.python_version(),
        'python_gil_enabled': bool(gil_enabled),
        'c_two_version': _pkg_version('c-two'),
        'fastdb4py_version': _pkg_version('fastdb4py'),
        'numpy_version': _pkg_version('numpy'),
        'c3_version': _c3_version(c3_bin),
        'git_commit': _git_short_head(),
        'headless': True,
        'ctwo_role_memory_budget_bytes': DEFAULT_ROLE_BUDGET_BYTES,
    }
    if include_ray:
        env['ray_version'] = _pkg_version('ray')
        env['ray_object_store_memory_bytes'] = RAY_OBJECT_STORE_BYTES
        env['ray_default_inline_threshold_bytes'] = 100 * KIB
    return env


# ---------------------------------------------------------------------------
# C-Two CRM contracts & spawned server processes
# ---------------------------------------------------------------------------


@cc.crm(namespace='bench.portable.coupling', version='0.1.0')
class FastDbCouplingContract:
    @cc.read
    @cc.transfer(input=FIELD_SPEC, output=FIELD_SPEC)
    def exchange(self, payload: Payload) -> Payload: ...


@cc.crm(namespace='bench.pickle.coupling', version='0.1.0')
class PickleCouplingContract:
    @cc.read
    def exchange(self, seq: int, arr: np.ndarray) -> tuple[int, np.ndarray]: ...


@cc.crm(namespace='bench.portable.field', version='0.1.0')
class FastDbFieldContract:
    @cc.read
    @cc.transfer(output=FIELD_SPEC)
    def snapshot_published(self) -> Payload: ...

    @cc.read
    @cc.transfer(output=FIELD_SPEC)
    def snapshot_fresh(self, seq: int) -> Payload: ...

    @cc.stream
    @cc.read
    @cc.transfer(output=FIELD_SPEC)
    def stream_published(self, count: int) -> Iterator[Payload]: ...

    @cc.stream
    @cc.read
    @cc.transfer(output=FIELD_SPEC)
    def stream_fresh(self, count: int) -> Iterator[Payload]: ...

    @cc.read
    def server_metrics(self) -> str: ...


@cc.crm(namespace='bench.portable.pubsub', version='0.1.0')
class FastDbPubSubContract:
    @cc.read
    @cc.transfer(output=FIELD_SPEC)
    def snapshot_published(self) -> Payload: ...

    @cc.read
    def emit_burst(self, count: int) -> str: ...

    @cc.read
    def server_metrics(self) -> str: ...

    @cc.topic
    @cc.transfer(output=FIELD_SPEC)
    def frames(self) -> Payload: ...


@cc.crm(namespace='bench.pickle.field', version='0.1.0')
class PickleFieldContract:
    @cc.read
    def snapshot_published(self) -> tuple[int, np.ndarray]: ...

    @cc.read
    def snapshot_fresh(self, seq: int) -> tuple[int, np.ndarray]: ...

    @cc.read
    def server_metrics(self) -> str: ...


@cc.crm(namespace='bench.portable.kostya', version='0.1.0')
class FastDbKostyaContract:
    @cc.read
    @cc.transfer(output=KOSTYA_SPEC)
    def coords(self) -> Payload: ...


@cc.crm(namespace='bench.pickle.kostya', version='0.1.0')
class PickleKostyaContract:
    @cc.read
    def coords(self) -> dict[str, object]: ...


class FastDbCouplingResource:
    def __init__(self, n_bytes: int) -> None:
        self._base = make_field_array(n_bytes)

    def exchange(self, payload: Payload) -> Payload:
        seq, count, total, _ = read_and_reduce_field_payload(payload)
        verify_field_result(seq, seq, count, int(self._base.size), total)
        payload.close()
        out_arr = stamp_field_array(self._base, seq + 1)
        out_payload, _ = build_field_payload(out_arr, seq + 1)
        return out_payload


class PickleCouplingResource:
    def __init__(self, n_bytes: int) -> None:
        self._base = make_field_array(n_bytes)

    def exchange(self, seq: int, arr: np.ndarray) -> tuple[int, np.ndarray]:
        verify_field_result(seq, seq, int(arr.size), int(self._base.size), float(arr.sum()))
        return (seq + 1, stamp_field_array(self._base, seq + 1))


class _PublishedFastDbField:
    def __init__(self, n_bytes: int, runtime: cc.Runtime) -> None:
        self._runtime = runtime
        self._base = make_field_array(n_bytes)
        published_arr = stamp_field_array(self._base, 1)
        self._published, self._report = build_field_payload(published_arr, 1)

    def snapshot_published(self) -> Payload:
        return self._published

    def server_metrics(self) -> str:
        return json.dumps({
            'peak_rss_mib': peak_rss_mib(),
            'memory_stats': self._runtime.memory_stats(),
            'fastdb_report': self._report,
            'encoded_bytes': len(self._published.binary_bytes()),
        })


class FastDbFieldResource(_PublishedFastDbField):
    def snapshot_fresh(self, seq: int) -> Payload:
        arr = stamp_field_array(self._base, seq)
        payload, _ = build_field_payload(arr, seq)
        return payload

    def stream_published(self, count: int) -> Iterator[Payload]:
        for _ in range(count):
            yield self._published

    def stream_fresh(self, count: int) -> Iterator[Payload]:
        for seq in range(1, count + 1):
            arr = stamp_field_array(self._base, seq)
            payload, _ = build_field_payload(arr, seq)
            try:
                yield payload
            finally:
                payload.close()

class FastDbPubSubResource(_PublishedFastDbField):
    def set_publisher(self, publisher: cc.Publisher[Payload]) -> None:
        self._publisher = publisher

    def emit_burst(self, count: int) -> str:
        t0 = time.perf_counter()
        seqs = [self._publisher.publish(self._published) for _ in range(count)]
        elapsed_s = time.perf_counter() - t0
        return json.dumps({
            'publish_elapsed_s': elapsed_s,
            'first_seq': seqs[0],
            'last_seq': seqs[-1],
        })


class PickleFieldResource:
    def __init__(self, n_bytes: int, runtime: cc.Runtime) -> None:
        self._runtime = runtime
        self._base = make_field_array(n_bytes)
        self._published = (1, stamp_field_array(self._base, 1))

    def snapshot_published(self) -> tuple[int, np.ndarray]:
        return self._published

    def snapshot_fresh(self, seq: int) -> tuple[int, np.ndarray]:
        return (seq, stamp_field_array(self._base, seq))

    def server_metrics(self) -> str:
        return json.dumps({
            'peak_rss_mib': peak_rss_mib(),
            'memory_stats': self._runtime.memory_stats(),
        })


class FastDbKostyaResource:
    def __init__(self, n: int) -> None:
        self._payload, self._report = build_kostya_fastdb_payload(n)

    def coords(self) -> Payload:
        return self._payload


class PickleKostyaResource:
    def __init__(self, n: int) -> None:
        self._data = build_kostya_numpy_dict(n)

    def coords(self) -> dict[str, object]:
        return self._data


def _ctwo_server_main(
    kind: str,
    param: int,
    endpoint_root: str | None,
    relay_url: str | None,
    ready_q: mp.Queue,
    stop_ev: mp.Event,
    topic_capacity: int | None = None,
) -> None:
    os.environ['C2_ENV_FILE'] = ''
    os.environ['C2_RELAY_ANCHOR_ADDRESS'] = ''
    runtime = cc.Runtime()
    if endpoint_root is not None and os.name != 'nt':
        runtime.set_local_endpoint(root=endpoint_root)
    runtime.set_server(memory_budget_bytes=DEFAULT_ROLE_BUDGET_BYTES)
    if relay_url is not None:
        runtime.set_relay_anchor(relay_url)
    try:
        conc = cc.ConcurrencyConfig(mode='read_parallel', max_workers=8)
        if kind == 'fastdb_coupling':
            runtime.register(FastDbCouplingContract, FastDbCouplingResource(param), name='bench', concurrency=conc)
        elif kind == 'pickle_coupling':
            runtime.register(PickleCouplingContract, PickleCouplingResource(param), name='bench', concurrency=conc)
        elif kind == 'fastdb_field':
            runtime.register(FastDbFieldContract, FastDbFieldResource(param, runtime), name='bench', concurrency=conc)
        elif kind == 'fastdb_pubsub':
            assert topic_capacity is not None
            pubsub = FastDbPubSubResource(param, runtime)
            registration = runtime.register(
                FastDbPubSubContract, pubsub, name='bench', concurrency=conc,
                topic_capacity={'frames': topic_capacity},
            )
            pubsub.set_publisher(registration.publisher('frames'))
        elif kind == 'pickle_field':
            runtime.register(PickleFieldContract, PickleFieldResource(param, runtime), name='bench', concurrency=conc)
        elif kind == 'fastdb_kostya':
            runtime.register(FastDbKostyaContract, FastDbKostyaResource(param), name='bench', concurrency=conc)
        elif kind == 'pickle_kostya':
            runtime.register(PickleKostyaContract, PickleKostyaResource(param), name='bench', concurrency=conc)
        else:
            raise ValueError(f'unknown server kind {kind!r}')
        ready_q.put(('ok', runtime.server_address()))
        while not stop_ev.is_set():
            stop_ev.wait(timeout=0.1)
    except BaseException as exc:
        ready_q.put(('err', repr(exc)))
        raise
    finally:
        runtime.shutdown(timeout=10.0)


class SpawnedCtwoServer:
    def __init__(
        self, kind: str, param: int, *, endpoint_root: str | None,
        relay_url: str | None = None, topic_capacity: int | None = None,
    ) -> None:
        self._kind = kind
        self._param = param
        self._endpoint_root = endpoint_root
        self._relay_url = relay_url
        self._topic_capacity = topic_capacity
        self._ctx = mp.get_context('spawn')
        self._ready_q = self._ctx.Queue()
        self._stop_ev = self._ctx.Event()
        self._proc: mp.Process | None = None
        self.address: str = ''

    def __enter__(self) -> SpawnedCtwoServer:
        self._proc = self._ctx.Process(
            target=_ctwo_server_main,
            args=(
                self._kind, self._param, self._endpoint_root, self._relay_url,
                self._ready_q, self._stop_ev, self._topic_capacity,
            ),
        )
        self._proc.start()
        try:
            status, value = self._ready_q.get(timeout=60.0)
            if status != 'ok':
                raise RuntimeError(f'C-Two server {self._kind} failed to start: {value}')
        except BaseException:
            self.__exit__(None, None, None)
            raise
        assert isinstance(value, str)
        self.address = value
        return self

    def __exit__(self, *_: object) -> None:
        self._stop_ev.set()
        if self._proc is not None:
            self._proc.join(timeout=15.0)
            if self._proc.is_alive():
                self._proc.terminate()
                self._proc.join(timeout=5.0)


def _free_tcp_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(('127.0.0.1', 0))
        return int(sock.getsockname()[1])


class SpawnedRelay:
    def __init__(
        self,
        c3_bin: str,
        ipc_root: str | None,
        *,
        seeds: list[str] | None = None,
        relay_id: str | None = None,
        memory_budget_bytes: int = DEFAULT_ROLE_BUDGET_BYTES,
    ) -> None:
        self._c3_bin = c3_bin
        self._ipc_root = ipc_root
        self._seeds = seeds
        self._relay_id = relay_id
        self._memory_budget_bytes = memory_budget_bytes
        self._proc: subprocess.Popen[str] | None = None
        self.url: str = ''
        self.peak_rss_mib: float | None = None

    def __enter__(self) -> SpawnedRelay:
        port = _free_tcp_port()
        bind = f'127.0.0.1:{port}'
        self.url = f'http://{bind}'
        args = [
            self._c3_bin, 'relay', '--bind', bind, '--advertise-url', self.url,
            '--memory-budget-bytes', str(self._memory_budget_bytes),
        ]
        if self._ipc_root is not None and os.name != 'nt':
            args.extend(['--ipc-root', self._ipc_root])
        if self._seeds:
            args.extend(['--seeds', ','.join(self._seeds)])
        if self._relay_id is not None:
            args.extend(['--relay-id', self._relay_id])
        env = os.environ.copy()
        env['C2_ENV_FILE'] = ''
        env.pop('C2_RELAY_SEEDS', None)
        env.pop('C2_RELAY_ID', None)
        env['NO_PROXY'] = '127.0.0.1,localhost'
        env['no_proxy'] = '127.0.0.1,localhost'
        self._proc = subprocess.Popen(
            args,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        deadline = time.monotonic() + 15.0
        while time.monotonic() < deadline:
            if self._proc.poll() is not None:
                stderr = self._proc.stderr.read() if self._proc.stderr else ''
                self.__exit__(None, None, None)
                raise RuntimeError(f'c3 relay exited prematurely: {stderr}')
            try:
                with _NO_PROXY_OPENER.open(f'{self.url}/health', timeout=1.0) as resp:
                    if resp.status == 200:
                        return self
            except Exception:
                time.sleep(0.05)
        self.__exit__(None, None, None)
        raise TimeoutError(f'c3 relay did not become healthy at {self.url}')

    def _reap_with_rss(self, proc: subprocess.Popen[str]) -> None:
        # Observe exit without reaping so wait4 can collect this child's usage.
        try:
            os.kill(proc.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 5.0
        while os.waitid(os.P_PID, proc.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is None:
            if time.monotonic() >= deadline:
                try:
                    os.kill(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                break
            time.sleep(0.01)
        _, status, usage = os.wait4(proc.pid, 0)
        proc.returncode = os.waitstatus_to_exitcode(status)
        self.peak_rss_mib = _ru_maxrss_mib(usage.ru_maxrss)

    def __exit__(self, *_: object) -> None:
        proc = self._proc
        if proc is None:
            return
        try:
            if proc.returncode is None and hasattr(os, 'wait4') and hasattr(os, 'waitid'):
                try:
                    self._reap_with_rss(proc)
                except OSError:
                    pass
            if proc.returncode is None:
                proc.terminate()
                try:
                    proc.wait(timeout=5.0)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=5.0)
        finally:
            if proc.stderr is not None:
                proc.stderr.close()


def _make_client_runtime(endpoint_root: str | None) -> cc.Runtime:
    runtime = cc.Runtime()
    if endpoint_root is not None and os.name != 'nt':
        runtime.set_local_endpoint(root=endpoint_root)
    runtime.set_client(memory_budget_bytes=DEFAULT_ROLE_BUDGET_BYTES)
    return runtime


# ---------------------------------------------------------------------------
# Suite 0: Single-process FastDB stage decomposition
# ---------------------------------------------------------------------------


def run_stages_suite(*, smoke: bool) -> dict[str, Any]:
    sizes = [256 * KIB] if smoke else [256 * KIB, 4 * MIB, 32 * MIB]
    warmup = 1 if smoke else 3
    iters = 3 if smoke else 11
    spec = _compiled('field')
    rows: list[dict[str, Any]] = []

    for n_bytes in sizes:
        base = make_field_array(n_bytes)
        arr = stamp_field_array(base, 1)
        raw = arr.tobytes()

        build_ms: list[float] = []
        exec_ms: list[float] = []
        bin_ms: list[float] = []
        open_copy_ms: list[float] = []
        open_ext_copy_ms: list[float] = []
        open_ext_direct_ms: list[float] = []
        reduce_ms: list[float] = []
        release_ms: list[float] = []
        encoded_bytes = 0
        report_mode = ''
        staging_bytes = 0

        gc.collect()
        for step in range(warmup + iters):
            t0 = time.perf_counter()
            builder = Builder.create(spec)
            builder.entry_begin(0, 1).value_u32(1)
            builder.entry_begin(1, 1).value_u32(1)
            builder.entry_begin(2, 1).value_u32(int(arr.size))
            builder.entry_begin(3, 1).value_bytes(raw)
            plan = builder.freeze()
            builder.close()
            t1 = time.perf_counter()

            built = plan.execute(BuildPolicy.ALLOW_STAGING)
            plan.close()
            t2 = time.perf_counter()

            blob = built.payload.binary_bytes()
            t3 = time.perf_counter()

            encoded_bytes = len(blob)
            report_mode = str(built.report.mode)
            staging_bytes = int(built.report.staging_bytes)

            mv = memoryview(blob)
            t4 = time.perf_counter()
            p_copy = Payload.open_copy(spec, bytes(mv))
            t5 = time.perf_counter()

            seq, count, total, _ = read_and_reduce_field_payload(p_copy)
            verify_field_result(seq, 1, count, int(arr.size), total)
            t6 = time.perf_counter()

            p_copy.close()
            t7 = time.perf_counter()

            # Paired decode probe: open_external with bytes(mv) vs existing bytes object.
            t8 = time.perf_counter()
            p_ext_copy = Payload.open_external(spec, ExternalBytes(bytes(mv)))
            t9 = time.perf_counter()
            p_ext_copy.close()

            t10 = time.perf_counter()
            p_ext_direct = Payload.open_external(spec, ExternalBytes(blob))
            t11 = time.perf_counter()
            p_ext_direct.close()

            built.payload.close()
            mv.release()
            del p_copy, p_ext_copy, p_ext_direct, built, blob, mv
            # fastdb4py 0.2.4 retains a small cyclic graph until cyclic GC runs;
            # collect between timed iterations so single-process stage timing does
            # not inflate driver RSS across unrelated suites.
            gc.collect()

            if step >= warmup:
                build_ms.append((t1 - t0) * 1e3)
                exec_ms.append((t2 - t1) * 1e3)
                bin_ms.append((t3 - t2) * 1e3)
                open_copy_ms.append((t5 - t4) * 1e3)
                reduce_ms.append((t6 - t5) * 1e3)
                release_ms.append((t7 - t6) * 1e3)
                open_ext_copy_ms.append((t9 - t8) * 1e3)
                open_ext_direct_ms.append((t11 - t10) * 1e3)

        rows.append({
            'logical_bytes': n_bytes,
            'encoded_bytes': encoded_bytes,
            'fastdb_execution_mode': report_mode,
            'fastdb_staging_bytes': staging_bytes,
            'samples': iters,
            'stages_p50_ms': {
                'build_and_freeze': round(nearest_rank(build_ms, 0.50), 4),
                'execute_plan': round(nearest_rank(exec_ms, 0.50), 4),
                'binary_bytes_export': round(nearest_rank(bin_ms, 0.50), 4),
                'open_copy_from_memoryview': round(nearest_rank(open_copy_ms, 0.50), 4),
                'open_external_from_memoryview': round(nearest_rank(open_ext_copy_ms, 0.50), 4),
                'open_external_from_bytes': round(nearest_rank(open_ext_direct_ms, 0.50), 4),
                'view_and_numpy_reduce': round(nearest_rank(reduce_ms, 0.50), 4),
                'release': round(nearest_rank(release_ms, 0.50), 4),
            },
            'copy_ledger_shm_unary': COPY_LEDGER_SHM_UNARY,
            'copy_ledger_shm_stream': COPY_LEDGER_SHM_STREAM,
        })

    return {'suite': 'stages', 'rows': rows}


# ---------------------------------------------------------------------------
# Suite 1: Small-packet bidirectional coupling exchange
# ---------------------------------------------------------------------------


def _run_ctwo_coupling(kind: str, n_bytes: int, warmup: int, iters: int) -> dict[str, Any]:
    endpoint_root = tempfile.mkdtemp(prefix='c2-bench-coupling-') if os.name != 'nt' else None
    try:
        with SpawnedCtwoServer(kind, n_bytes, endpoint_root=endpoint_root) as srv:
            client = _make_client_runtime(endpoint_root)
            contract = FastDbCouplingContract if kind == 'fastdb_coupling' else PickleCouplingContract
            t_conn0 = time.perf_counter()
            proxy = client.connect(contract, name='bench', address=srv.address)
            connect_ms = (time.perf_counter() - t_conn0) * 1e3
            base = make_field_array(n_bytes)
            encoded_bytes = 0
            fastdb_report: dict[str, Any] = {}
            samples_ms: list[float] = []
            gc.collect()
            rss_before = live_rss_mib()
            try:
                for i in range(warmup + iters):
                    seq = i + 1
                    arr = stamp_field_array(base, seq)
                    t0 = time.perf_counter()
                    if kind == 'fastdb_coupling':
                        req, rep = build_field_payload(arr, seq)
                        if i == 0:
                            fastdb_report = rep
                            encoded_bytes = len(req.binary_bytes())
                        try:
                            resp = proxy.exchange(req)
                        finally:
                            req.close()
                        try:
                            r_seq, r_cnt, r_sum, _ = read_and_reduce_field_payload(resp)
                            verify_field_result(r_seq, seq + 1, r_cnt, int(base.size), r_sum)
                        finally:
                            resp.close()
                    else:
                        r_seq, r_arr = proxy.exchange(seq, arr)
                        verify_field_result(r_seq, seq + 1, int(r_arr.size), int(base.size), float(r_arr.sum()))
                        del r_arr
                    dt_ms = (time.perf_counter() - t0) * 1e3
                    if i >= warmup:
                        samples_ms.append(dt_ms)
                rss_after_before_gc = live_rss_mib()
                mem = client.memory_stats()
            finally:
                cc.close(proxy)
                outcome = client.shutdown(timeout=10.0)
                assert outcome['completed'] is True
                gc.collect()
            row: dict[str, Any] = {
                'arm': 'ctwo-fastdb-unary' if kind == 'fastdb_coupling' else 'ctwo-pickle-arrays',
                'logical_bytes': n_bytes,
                'encoded_bytes': encoded_bytes if encoded_bytes else None,
                'connect_ms': round(connect_ms, 3),
                'client_live_rss_before_mib': rss_before,
                'client_live_rss_after_mib': rss_after_before_gc,
                'client_peak_rss_mib': peak_rss_mib(),
                'client_holds_active': mem['holds']['active_holds'],
                'transport_path': 'direct_ipc',
                'crosses_16k_shm_threshold': (encoded_bytes >= 16 * KIB) if encoded_bytes else (n_bytes >= 16 * KIB),
                **summarize_ms(samples_ms, payload_bytes=n_bytes, bidirectional=True),
            }
            if fastdb_report:
                row['fastdb_execution_mode'] = fastdb_report['execution_mode']
                row['fastdb_staging_bytes'] = fastdb_report['staging_bytes']
                row['copy_ledger_one_way'] = COPY_LEDGER_SHM_UNARY if encoded_bytes >= 16 * KIB else 'inline_ipc_frame'
            return row
    finally:
        if endpoint_root is not None:
            shutil.rmtree(endpoint_root, ignore_errors=True)


def run_coupling_suite(*, smoke: bool, include_ray: bool) -> dict[str, Any]:
    sizes = [16 * KIB] if smoke else [4 * KIB, 16 * KIB, 64 * KIB]
    warmup = 2 if smoke else 10
    iters = 8 if smoke else 60
    rows: list[dict[str, Any]] = []

    for n_bytes in sizes:
        rows.append(_run_ctwo_coupling('fastdb_coupling', n_bytes, warmup, iters))
        if n_bytes == 16 * KIB:
            rows.append(_run_ctwo_coupling('pickle_coupling', n_bytes, warmup, iters))
            if include_ray:
                rows.extend(_run_ray_coupling_arms(n_bytes, warmup, iters))

    return {'suite': 'coupling', 'rows': rows}


# ---------------------------------------------------------------------------
# Suite 2: Full-field grid snapshot & live stream
# ---------------------------------------------------------------------------


def _run_ctwo_field_arm(
    *,
    arm: str,
    n_bytes: int,
    mode: str,  # 'published' or 'fresh'
    warmup: int,
    iters: int,
    c3_bin: str | None = None,
) -> dict[str, Any]:
    is_pickle = arm == 'ctwo-pickle-arrays'
    is_relay = arm == 'ctwo-fastdb-relay-stream'
    is_stream = arm in ('ctwo-fastdb-stream', 'ctwo-fastdb-relay-stream')
    is_hold = arm == 'ctwo-fastdb-hold'
    server_kind = 'pickle_field' if is_pickle else 'fastdb_field'
    contract = PickleFieldContract if is_pickle else FastDbFieldContract
    endpoint_root = tempfile.mkdtemp(prefix='c2-bench-field-') if os.name != 'nt' else None

    try:
        relay_ctx = SpawnedRelay(c3_bin, endpoint_root) if is_relay else None
        if relay_ctx is not None:
            assert c3_bin is not None
            relay_ctx.__enter__()
        try:
            relay_url = relay_ctx.url if relay_ctx is not None else None
            with SpawnedCtwoServer(server_kind, n_bytes, endpoint_root=endpoint_root, relay_url=relay_url) as srv:
                client = _make_client_runtime(endpoint_root)
                connect_addr = relay_url if is_relay else srv.address
                assert connect_addr is not None
                t_conn0 = time.perf_counter()
                proxy = client.connect(contract, name='bench', address=connect_addr)
                connect_ms = (time.perf_counter() - t_conn0) * 1e3
                observed_path = proxy.client.observed_path
                count_elems = n_bytes // 8
                samples_ms: list[float] = []
                extract_reduce_ms: list[float] = []
                first_item_ms: float | None = None
                stream_total_s: float | None = None
                srv_metrics: dict[str, Any] = {}
                gc.collect()
                rss_before = live_rss_mib()
                try:
                    if is_stream:
                        # Warm up one short stream first.
                        w_stream = (
                            proxy.stream_published(warmup)
                            if mode == 'published'
                            else proxy.stream_fresh(warmup)
                        )
                        try:
                            for idx, item in enumerate(w_stream, 1):
                                exp_seq = 1 if mode == 'published' else idx
                                try:
                                    r_seq, r_cnt, r_sum, _ = read_and_reduce_field_payload(item)
                                    verify_field_result(r_seq, exp_seq, r_cnt, count_elems, r_sum)
                                finally:
                                    item.close()
                        finally:
                            assert w_stream.close(timeout=10.0)['completed'] is True

                        t_open = time.perf_counter()
                        stream = (
                            proxy.stream_published(iters)
                            if mode == 'published'
                            else proxy.stream_fresh(iters)
                        )
                        try:
                            prev = t_open
                            for idx, item in enumerate(stream, 1):
                                exp_seq = 1 if mode == 'published' else idx
                                try:
                                    r_seq, r_cnt, r_sum, er_ms = read_and_reduce_field_payload(item)
                                    verify_field_result(r_seq, exp_seq, r_cnt, count_elems, r_sum)
                                finally:
                                    item.close()
                                now = time.perf_counter()
                                dt_ms = (now - prev) * 1e3
                                if idx == 1:
                                    first_item_ms = dt_ms
                                samples_ms.append(dt_ms)
                                extract_reduce_ms.append(er_ms)
                                prev = now
                            stream_total_s = time.perf_counter() - t_open
                        finally:
                            receipt = stream.close(timeout=15.0)
                            assert receipt['completed'] is True
                            assert receipt['error'] is None
                            assert receipt['complete_items'] == iters
                    else:
                        for i in range(warmup + iters):
                            exp_seq = 1 if mode == 'published' else (i + 1)
                            t0 = time.perf_counter()
                            if is_pickle:
                                r_seq, r_arr = (
                                    proxy.snapshot_published()
                                    if mode == 'published'
                                    else proxy.snapshot_fresh(exp_seq)
                                )
                                t_er0 = time.perf_counter()
                                total = float(r_arr.sum())
                                er_ms = (time.perf_counter() - t_er0) * 1e3
                                verify_field_result(r_seq, exp_seq, int(r_arr.size), count_elems, total)
                                del r_arr
                            elif is_hold:
                                fn = (
                                    cc.hold(proxy.snapshot_published)
                                    if mode == 'published'
                                    else cc.hold(proxy.snapshot_fresh)
                                )
                                held = fn() if mode == 'published' else fn(exp_seq)
                                try:
                                    r_seq, r_cnt, r_sum, er_ms = read_and_reduce_field_payload(held.value)
                                    verify_field_result(r_seq, exp_seq, r_cnt, count_elems, r_sum)
                                finally:
                                    held.release()
                            else:
                                resp = (
                                    proxy.snapshot_published()
                                    if mode == 'published'
                                    else proxy.snapshot_fresh(exp_seq)
                                )
                                try:
                                    r_seq, r_cnt, r_sum, er_ms = read_and_reduce_field_payload(resp)
                                    verify_field_result(r_seq, exp_seq, r_cnt, count_elems, r_sum)
                                finally:
                                    resp.close()
                            dt_ms = (time.perf_counter() - t0) * 1e3
                            if i >= warmup:
                                samples_ms.append(dt_ms)
                                extract_reduce_ms.append(er_ms)
                    srv_metrics = json.loads(proxy.server_metrics())
                    rss_after_before_gc = live_rss_mib()
                    client_mem = client.memory_stats()
                finally:
                    cc.close(proxy)
                    outcome = client.shutdown(timeout=10.0)
                    assert outcome['completed'] is True
                    gc.collect()
                row: dict[str, Any] = {
                    'arm': arm,
                    'frame_mode': mode,
                    'logical_bytes': n_bytes,
                    'encoded_bytes': srv_metrics.get('encoded_bytes'),
                    'transport_path': observed_path,
                    'connect_ms': round(connect_ms, 3),
                    'extract_and_reduce_p50_ms': round(nearest_rank(extract_reduce_ms, 0.50), 4),
                    'client_live_rss_before_mib': rss_before,
                    'client_live_rss_after_mib': rss_after_before_gc,
                    'client_peak_rss_mib': peak_rss_mib(),
                    'server_peak_rss_mib': srv_metrics.get('peak_rss_mib'),
                    'client_holds_active': client_mem['holds']['active_holds'],
                    **summarize_ms(samples_ms, payload_bytes=n_bytes, bidirectional=False),
                }
                if first_item_ms is not None and stream_total_s is not None:
                    row['first_item_ms'] = round(first_item_ms, 4)
                    row['steady_stream_frames_per_s'] = round(iters / stream_total_s, 2)
                    row['steady_stream_gib_s'] = round((iters * n_bytes) / stream_total_s / (1024 ** 3), 4)
                if 'fastdb_report' in srv_metrics:
                    row['fastdb_execution_mode'] = srv_metrics['fastdb_report']['execution_mode']
                    row['fastdb_staging_bytes'] = srv_metrics['fastdb_report']['staging_bytes']
                    if is_relay:
                        row['copy_ledger_one_way'] = 'explicit_relay_http_stream'
                    elif is_stream:
                        row['copy_ledger_one_way'] = COPY_LEDGER_SHM_STREAM
                    else:
                        row['copy_ledger_one_way'] = COPY_LEDGER_SHM_UNARY
                return row
        finally:
            if relay_ctx is not None:
                relay_ctx.__exit__(None, None, None)
    finally:
        if endpoint_root is not None:
            shutil.rmtree(endpoint_root, ignore_errors=True)


def run_field_suite(*, smoke: bool, include_ray: bool, c3_bin: str | None) -> dict[str, Any]:
    sizes = [256 * KIB, 1 * MIB] if smoke else [256 * KIB, 1 * MIB, 4 * MIB, 16 * MIB, 32 * MIB]
    compare_sizes = {1 * MIB} if smoke else {4 * MIB, 32 * MIB}
    rows: list[dict[str, Any]] = []

    for n_bytes in sizes:
        warmup = 1 if smoke else (3 if n_bytes <= 4 * MIB else 2)
        iters = 4 if smoke else (25 if n_bytes <= 4 * MIB else 12)
        for mode in ('published', 'fresh'):
            rows.append(_run_ctwo_field_arm(arm='ctwo-fastdb-unary', n_bytes=n_bytes, mode=mode, warmup=warmup, iters=iters))
            rows.append(_run_ctwo_field_arm(arm='ctwo-fastdb-stream', n_bytes=n_bytes, mode=mode, warmup=warmup, iters=iters))
            if n_bytes in compare_sizes:
                rows.append(_run_ctwo_field_arm(arm='ctwo-fastdb-hold', n_bytes=n_bytes, mode=mode, warmup=warmup, iters=iters))
                rows.append(_run_ctwo_field_arm(arm='ctwo-pickle-arrays', n_bytes=n_bytes, mode=mode, warmup=warmup, iters=iters))
                if c3_bin is not None:
                    rows.append(_run_ctwo_field_arm(arm='ctwo-fastdb-relay-stream', n_bytes=n_bytes, mode=mode, warmup=warmup, iters=iters, c3_bin=c3_bin))
                if include_ray:
                    rows.extend(_run_ray_field_arms(n_bytes=n_bytes, mode=mode, warmup=warmup, iters=iters))

    return {'suite': 'field', 'rows': rows}


# ---------------------------------------------------------------------------
# Suite 3: Multi-subscriber fan-out amplification baseline (R = 1, 2, 4)
# ---------------------------------------------------------------------------


def _fanout_subscriber_main(
    address: str,
    endpoint_root: str | None,
    n_bytes: int,
    frames: int,
    barrier: Any,
    out_q: mp.Queue,
) -> None:
    os.environ['C2_ENV_FILE'] = ''
    os.environ['C2_RELAY_ANCHOR_ADDRESS'] = ''
    client = _make_client_runtime(endpoint_root)
    proxy = None
    try:
        proxy = client.connect(FastDbFieldContract, name='bench', address=address)
        observed_path = proxy.client.observed_path
        count_elems = n_bytes // 8
        # Warm up connection before synchronized start.
        snap = proxy.snapshot_published()
        try:
            s, c, tot, _ = read_and_reduce_field_payload(snap)
            verify_field_result(s, 1, c, count_elems, tot)
        finally:
            snap.close()

        barrier.wait()
        t0 = time.perf_counter()
        stream = proxy.stream_published(frames)
        frame_ms: list[float] = []
        try:
            prev = t0
            for item in stream:
                try:
                    s, c, tot, _ = read_and_reduce_field_payload(item)
                    verify_field_result(s, 1, c, count_elems, tot)
                finally:
                    item.close()
                now = time.perf_counter()
                frame_ms.append((now - prev) * 1e3)
                prev = now
            elapsed_s = time.perf_counter() - t0
        finally:
            receipt = stream.close(timeout=15.0)
            assert receipt['completed'] is True
            assert receipt['complete_items'] == frames
        out_q.put((
            'ok',
            {
                'observed_path': observed_path,
                'elapsed_s': elapsed_s,
                'p50_ms': nearest_rank(frame_ms, 0.50),
                'p95_ms': nearest_rank(frame_ms, 0.95),
                'peak_rss_mib': peak_rss_mib(),
            },
        ))
    except BaseException as exc:
        out_q.put(('err', repr(exc)))
    finally:
        if proxy is not None:
            cc.close(proxy)
        client.shutdown(timeout=10.0)


def _run_fanout_case(
    *,
    transport: str,  # 'direct_ipc' or 'explicit_relay'
    n_bytes: int,
    readers: int,
    frames: int,
    c3_bin: str | None,
) -> dict[str, Any]:
    endpoint_root = tempfile.mkdtemp(prefix='c2-bench-fanout-') if os.name != 'nt' else None
    ctx = mp.get_context('spawn')
    try:
        relay_ctx = SpawnedRelay(c3_bin, endpoint_root) if transport == 'explicit_relay' else None
        if relay_ctx is not None:
            assert c3_bin is not None
            relay_ctx.__enter__()
        try:
            relay_url = relay_ctx.url if relay_ctx is not None else None
            with SpawnedCtwoServer('fastdb_field', n_bytes, endpoint_root=endpoint_root, relay_url=relay_url) as srv:
                target_addr = relay_url if transport == 'explicit_relay' else srv.address
                assert target_addr is not None
                barrier = ctx.Barrier(readers)
                out_q = ctx.Queue()
                procs = [
                    ctx.Process(
                        target=_fanout_subscriber_main,
                        args=(target_addr, endpoint_root, n_bytes, frames, barrier, out_q),
                    )
                    for _ in range(readers)
                ]
                t_wall0 = time.perf_counter()
                for proc in procs:
                    proc.start()
                results = [out_q.get(timeout=90.0) for _ in range(readers)]
                for proc in procs:
                    proc.join(timeout=15.0)
                    if proc.is_alive():
                        proc.terminate()
                        proc.join(timeout=5.0)
                wall_s = time.perf_counter() - t_wall0
                for status, payload in results:
                    if status != 'ok':
                        raise RuntimeError(f'fanout subscriber failed: {payload}')
                sub_rows = [payload for _, payload in results]
                max_elapsed = max(r['elapsed_s'] for r in sub_rows)
                per_reader_fps = frames / max_elapsed if max_elapsed > 0 else 0.0
                total_logical_bytes_moved = readers * frames * n_bytes
                # Collect server peak RSS cleanly.
                probe_client = _make_client_runtime(endpoint_root)
                probe_proxy = probe_client.connect(FastDbFieldContract, name='bench', address=srv.address)
                try:
                    srv_metrics = json.loads(probe_proxy.server_metrics())
                finally:
                    cc.close(probe_proxy)
                    probe_client.shutdown(timeout=10.0)
                return {
                    'transport': transport,
                    'observed_paths': sorted({r['observed_path'] for r in sub_rows}),
                    'logical_bytes': n_bytes,
                    'encoded_bytes': srv_metrics.get('encoded_bytes'),
                    'readers': readers,
                    'frames_per_reader': frames,
                    'max_reader_elapsed_s': round(max_elapsed, 4),
                    'per_reader_p50_ms': round(statistics.fmean(r['p50_ms'] for r in sub_rows), 4),
                    'per_reader_p95_ms': round(max(r['p95_ms'] for r in sub_rows), 4),
                    'per_reader_effective_fps': round(per_reader_fps, 2),
                    'aggregate_moved_gib': round(total_logical_bytes_moved / (1024 ** 3), 4),
                    'aggregate_gib_s': round((total_logical_bytes_moved / max_elapsed) / (1024 ** 3), 4),
                    'redundant_copy_multiplier': readers,
                    'server_peak_rss_mib': srv_metrics.get('peak_rss_mib'),
                    'server_pool_peak_mib': round(srv_metrics['memory_stats']['server']['memory']['peak_bytes'] / MIB, 2),
                    'max_client_peak_rss_mib': max(r['peak_rss_mib'] for r in sub_rows),
                    'wall_with_spawn_s': round(wall_s, 3),
                }
        finally:
            if relay_ctx is not None:
                relay_ctx.__exit__(None, None, None)
    finally:
        if endpoint_root is not None:
            shutil.rmtree(endpoint_root, ignore_errors=True)


def run_fanout_suite(*, smoke: bool, c3_bin: str | None) -> dict[str, Any]:
    sizes = [256 * KIB] if smoke else [4 * MIB, 32 * MIB]
    reader_counts = [1, 2] if smoke else [1, 2, 4]
    transports = ['direct_ipc'] + (['explicit_relay'] if c3_bin is not None else [])
    rows: list[dict[str, Any]] = []

    for transport in transports:
        for n_bytes in sizes:
            frames = 3 if smoke else (16 if n_bytes <= 4 * MIB else 8)
            for readers in reader_counts:
                rows.append(
                    _run_fanout_case(
                        transport=transport,
                        n_bytes=n_bytes,
                        readers=readers,
                        frames=frames,
                        c3_bin=c3_bin,
                    )
                )

    return {'suite': 'fanout', 'rows': rows}


# ---------------------------------------------------------------------------
# Publish/subscribe fan-out over IPC and relay mesh
# ---------------------------------------------------------------------------


def _pubsub_fanout_subscriber_main(
    address: str,
    endpoint_root: str | None,
    n_bytes: int,
    frames: int,
    barrier: Any,
    out_q: mp.Queue,
) -> None:
    client = _make_client_runtime(endpoint_root)
    proxy = None
    try:
        proxy = client.connect(FastDbPubSubContract, name='bench', address=address)
        count_elems = n_bytes // 8
        snap = proxy.snapshot_published()
        try:
            s, c, tot, _ = read_and_reduce_field_payload(snap)
            verify_field_result(s, 1, c, count_elems, tot)
        finally:
            snap.close()

        sub = proxy.frames(start='next', on_lag='error')
        frame_ms: list[float] = []
        dropped_total = 0
        try:
            # READY reports the first deliverable sequence for start='next'.
            first_seq = sub.start_seq
            barrier.wait(timeout=30.0)
            t0 = prev = time.perf_counter()
            for expected_seq in range(first_seq, first_seq + frames):
                delivery = next(sub)
                try:
                    assert delivery.dropped == 0, delivery.dropped
                    assert delivery.seq == expected_seq, (delivery.seq, expected_seq)
                    dropped_total += delivery.dropped
                    s, c, tot, _ = read_and_reduce_field_payload(delivery.value)
                    verify_field_result(s, 1, c, count_elems, tot)
                finally:
                    delivery.value.close()
                now = time.perf_counter()
                frame_ms.append((now - prev) * 1e3)
                prev = now
            elapsed_s = prev - t0
        finally:
            receipt = sub.close(timeout=15.0)
            assert receipt['completed'] is True, receipt

        out_q.put(('ok', {
            'observed_path': proxy.client.observed_path,
            'elapsed_s': elapsed_s,
            'p50_ms': nearest_rank(frame_ms, 0.50),
            'p95_ms': nearest_rank(frame_ms, 0.95),
            'peak_rss_mib': peak_rss_mib(),
            'dropped_total': dropped_total,
        }))
    except BaseException as exc:
        out_q.put(('err', repr(exc)))
        barrier.abort()
    finally:
        if proxy is not None:
            cc.close(proxy)
        client.shutdown(timeout=10.0)


def _wait_for_pubsub_route(address: str, endpoint_root: str | None) -> None:
    client = _make_client_runtime(endpoint_root)
    deadline = time.monotonic() + 10.0
    last_error = None
    try:
        while (remaining := deadline - time.monotonic()) > 0:
            try:
                proxy = client.connect(FastDbPubSubContract, name='bench', address=address, timeout=remaining)
                try:
                    budget = max(0.0, deadline - time.monotonic())
                    cc.with_call_options(proxy, timeout=budget).server_metrics()
                    return
                finally:
                    cc.close(proxy)
            except cc.error.CCBaseError as exc:
                last_error = exc
                time.sleep(min(0.05, max(0.0, deadline - time.monotonic())))
        raise TimeoutError('pub/sub route did not become reachable through Relay B') from last_error
    finally:
        client.shutdown(timeout=10.0)


def _run_pubsub_fanout_case(
    *, transport: str, n_bytes: int, readers: int, frames: int, c3_bin: str | None,
) -> dict[str, Any]:
    ctx = mp.get_context('spawn')
    # Keep the full burst within the history ring's N - 1 slots.
    topic_capacity = frames + 1
    relay_a = relay_b = None
    with ExitStack() as stack:
        root_a = stack.enter_context(tempfile.TemporaryDirectory(prefix='c2-bench-pub-a-')) if os.name != 'nt' else None
        if transport in ('explicit_relay', 'dual_relay'):
            assert c3_bin is not None
            relay_a = stack.enter_context(SpawnedRelay(
                c3_bin, root_a, relay_id='relay-a' if transport == 'dual_relay' else None,
            ))
        srv = stack.enter_context(SpawnedCtwoServer(
            'fastdb_pubsub', n_bytes, endpoint_root=root_a,
            relay_url=relay_a.url if relay_a is not None else None,
            topic_capacity=topic_capacity,
        ))
        reader_root = root_a
        target = relay_a.url if relay_a is not None else srv.address
        if transport == 'dual_relay':
            assert c3_bin is not None and relay_a is not None
            if os.name != 'nt':
                reader_root = stack.enter_context(tempfile.TemporaryDirectory(prefix='c2-bench-pub-b-'))
            relay_b = stack.enter_context(SpawnedRelay(c3_bin, reader_root, seeds=[relay_a.url], relay_id='relay-b'))
            target = relay_b.url
            _wait_for_pubsub_route(target, reader_root)

        coord = _make_client_runtime(root_a)
        stack.callback(coord.shutdown, timeout=10.0)
        coord_proxy = coord.connect(FastDbPubSubContract, name='bench', address=srv.address)
        stack.callback(cc.close, coord_proxy)
        barrier = ctx.Barrier(readers + 1)
        out_q = ctx.Queue()
        stack.callback(out_q.close)
        procs = []
        sub_rows = []
        t_wall0 = time.perf_counter()
        try:
            for _ in range(readers):
                proc = ctx.Process(
                    target=_pubsub_fanout_subscriber_main,
                    args=(target, reader_root, n_bytes, frames, barrier, out_q),
                )
                proc.start()
                procs.append(proc)
            try:
                barrier.wait(timeout=30.0)
            except BrokenBarrierError as exc:
                try:
                    _, detail = out_q.get(timeout=1.0)
                except Empty:
                    detail = 'subscription setup timed out'
                raise RuntimeError(f'pub/sub fanout barrier failed: {detail}') from exc
            burst = json.loads(coord_proxy.emit_burst(frames))
            assert (burst['first_seq'], burst['last_seq']) == (1, frames), burst
            deadline = time.monotonic() + 60.0
            for _ in range(readers):
                status, row = out_q.get(timeout=max(0.0, deadline - time.monotonic()))
                if status != 'ok':
                    raise RuntimeError(f'pub/sub fanout subscriber failed: {row}')
                sub_rows.append(row)
            for proc in procs:
                proc.join(timeout=15.0)
                if proc.exitcode != 0:
                    raise RuntimeError(f'pub/sub fanout subscriber exit code: {proc.exitcode}')
        finally:
            for proc in procs:
                if proc.is_alive():
                    proc.kill()
                    proc.join()
                proc.close()
        wall_s = time.perf_counter() - t_wall0
        srv_metrics = json.loads(coord_proxy.server_metrics())

    max_elapsed = max(row['elapsed_s'] for row in sub_rows)
    moved_gib = readers * frames * n_bytes / (1024 ** 3)
    dropped_total = sum(row['dropped_total'] for row in sub_rows)
    assert dropped_total == 0, dropped_total
    return {
        'transport': transport,
        'observed_paths': sorted({row['observed_path'] for row in sub_rows}),
        'logical_bytes': n_bytes,
        'encoded_bytes': srv_metrics['encoded_bytes'],
        'readers': readers,
        'frames_per_reader': frames,
        'topic_capacity': topic_capacity,
        'publish_elapsed_s': burst['publish_elapsed_s'],
        'max_reader_elapsed_s': round(max_elapsed, 4),
        'per_reader_p50_ms': round(statistics.fmean(row['p50_ms'] for row in sub_rows), 4),
        'per_reader_p95_ms': round(max(row['p95_ms'] for row in sub_rows), 4),
        'per_reader_effective_fps': round(frames / max_elapsed, 2),
        'aggregate_moved_gib': round(moved_gib, 4),
        'aggregate_gib_s': round(moved_gib / max_elapsed, 4),
        'server_write_multiplier': 1,
        'dropped_total': dropped_total,
        'server_peak_rss_mib': srv_metrics['peak_rss_mib'],
        'server_pool_peak_mib': round(srv_metrics['memory_stats']['server']['memory']['peak_bytes'] / MIB, 2),
        'max_client_peak_rss_mib': max(row['peak_rss_mib'] for row in sub_rows),
        'relay_a_peak_rss_mib': relay_a.peak_rss_mib if relay_a is not None else None,
        'relay_b_peak_rss_mib': relay_b.peak_rss_mib if relay_b is not None else None,
        'wall_with_spawn_s': round(wall_s, 3),
    }


def run_pubsub_fanout_suite(*, smoke: bool, c3_bin: str | None) -> dict[str, Any]:
    sizes = [256 * KIB] if smoke else [4 * MIB, 32 * MIB]
    reader_counts = [1, 2] if smoke else [1, 2, 4]
    transports = ['direct_ipc'] + (['explicit_relay', 'dual_relay'] if c3_bin is not None else [])
    rows: list[dict[str, Any]] = []
    for transport in transports:
        for n_bytes in sizes:
            frames = 3 if smoke else (16 if n_bytes <= 4 * MIB else 8)
            for readers in reader_counts:
                rows.append(_run_pubsub_fanout_case(
                    transport=transport, n_bytes=n_bytes, readers=readers, frames=frames, c3_bin=c3_bin,
                ))
    return {'suite': 'pubsub_fanout', 'rows': rows}


# ---------------------------------------------------------------------------
# Suite 4: Structured coordinates replacement baseline (Kostya)
# ---------------------------------------------------------------------------


def _run_ctwo_kostya(kind: str, n: int, warmup: int, iters: int) -> dict[str, Any]:
    endpoint_root = tempfile.mkdtemp(prefix='c2-bench-kostya-') if os.name != 'nt' else None
    is_fastdb = kind == 'fastdb_kostya'
    contract = FastDbKostyaContract if is_fastdb else PickleKostyaContract
    try:
        with SpawnedCtwoServer(kind, n, endpoint_root=endpoint_root) as srv:
            client = _make_client_runtime(endpoint_root)
            proxy = client.connect(contract, name='bench', address=srv.address)
            rpc_ms: list[float] = []
            extract_ms: list[float] = []
            reduce_ms: list[float] = []
            total_ms: list[float] = []
            encoded_bytes: int | None = None
            gc.collect()
            rss_before = live_rss_mib()
            try:
                for i in range(warmup + iters):
                    t0 = time.perf_counter()
                    res = proxy.coords()
                    t1 = time.perf_counter()
                    if is_fastdb:
                        if encoded_bytes is None:
                            encoded_bytes = len(res.binary_bytes())
                        try:
                            _, ex_ms, red_ms = reduce_kostya_fastdb(res, n)
                        finally:
                            res.close()
                    else:
                        _, ex_ms, red_ms = reduce_kostya_arrays(res, n)
                        del res
                    t2 = time.perf_counter()
                    if i >= warmup:
                        rpc_ms.append((t1 - t0) * 1e3)
                        extract_ms.append(ex_ms)
                        reduce_ms.append(red_ms)
                        total_ms.append((t2 - t0) * 1e3)
                rss_after_before_gc = live_rss_mib()
            finally:
                cc.close(proxy)
                outcome = client.shutdown(timeout=10.0)
                assert outcome['completed'] is True
                gc.collect()
            logical_bytes = n * (4 + 8 * 3 + 2)
            return {
                'arm': 'ctwo-fastdb-packed' if is_fastdb else 'ctwo-pickle-arrays',
                'rows': n,
                'logical_numeric_bytes': logical_bytes,
                'encoded_bytes': encoded_bytes,
                'rpc_p50_ms': round(nearest_rank(rpc_ms, 0.50), 4),
                'extract_p50_ms': round(nearest_rank(extract_ms, 0.50), 4),
                'reduce_p50_ms': round(nearest_rank(reduce_ms, 0.50), 4),
                'client_live_rss_before_mib': rss_before,
                'client_live_rss_after_mib': rss_after_before_gc,
                'client_peak_rss_mib': peak_rss_mib(),
                **summarize_ms(total_ms, payload_bytes=logical_bytes, bidirectional=False),
            }
    finally:
        if endpoint_root is not None:
            shutil.rmtree(endpoint_root, ignore_errors=True)


def run_kostya_suite(*, smoke: bool, include_ray: bool) -> dict[str, Any]:
    row_counts = [1_000] if smoke else [1_000, 100_000, 1_000_000]
    rows: list[dict[str, Any]] = []
    for n in row_counts:
        warmup = 1 if smoke else (3 if n <= 100_000 else 2)
        iters = 3 if smoke else (25 if n <= 100_000 else 10)
        rows.append(_run_ctwo_kostya('fastdb_kostya', n, warmup, iters))
        rows.append(_run_ctwo_kostya('pickle_kostya', n, warmup, iters))
        if include_ray:
            rows.append(_run_ray_kostya_arm(n, warmup, iters))
    return {'suite': 'kostya', 'rows': rows}


# ---------------------------------------------------------------------------
# Optional Ray 2.56+ counterpart arms (detached named actor & Compiled Graphs)
# ---------------------------------------------------------------------------


def _ensure_ray_started() -> Any:
    os.environ.setdefault('RAY_ENABLE_UV_RUN_RUNTIME_ENV', '0')
    import ray  # noqa: PLC0415

    if not ray.is_initialized():
        ray.init(
            num_cpus=2,
            object_store_memory=RAY_OBJECT_STORE_BYTES,
            include_dashboard=False,
            ignore_reinit_error=True,
            logging_level='ERROR',
            _system_config={'automatic_object_spilling_enabled': False},
        )
    return ray


def _shutdown_ray_if_running() -> None:
    if 'ray' in sys.modules:
        import ray  # noqa: PLC0415

        if ray.is_initialized():
            ray.shutdown()


def _run_ray_coupling_arms(n_bytes: int, warmup: int, iters: int) -> list[dict[str, Any]]:
    ray = _ensure_ray_started()
    from ray.dag import InputNode  # noqa: PLC0415
    from ray.dag.context import DAGContext  # noqa: PLC0415

    @ray.remote
    class RayCouplingActor:
        def __init__(self, size: int) -> None:
            self._base = make_field_array(size)

        def exchange(self, packet: tuple[int, np.ndarray]) -> tuple[int, np.ndarray]:
            seq, arr = packet
            verify_field_result(seq, seq, int(arr.size), int(self._base.size), float(arr.sum()))
            return (seq + 1, stamp_field_array(self._base, seq + 1))

    ns = f'bench-coupling-{os.getpid()}'
    actor_name = f'coupling-{n_bytes}-{time.time_ns()}'
    t_init0 = time.perf_counter()
    _ = RayCouplingActor.options(name=actor_name, namespace=ns, lifetime='detached').remote(n_bytes)
    actor = ray.get_actor(actor_name, namespace=ns)
    base = make_field_array(n_bytes)
    ray.get(actor.exchange.remote((0, stamp_field_array(base, 0))))
    actor_init_ms = (time.perf_counter() - t_init0) * 1e3

    out_rows: list[dict[str, Any]] = []
    try:
        # 1. Detached named actor
        samples_ms: list[float] = []
        for i in range(warmup + iters):
            seq = i + 1
            arr = stamp_field_array(base, seq)
            t0 = time.perf_counter()
            r_seq, r_arr = ray.get(actor.exchange.remote((seq, arr)))
            verify_field_result(r_seq, seq + 1, int(r_arr.size), int(base.size), float(r_arr.sum()))
            del r_arr
            dt_ms = (time.perf_counter() - t0) * 1e3
            if i >= warmup:
                samples_ms.append(dt_ms)
        out_rows.append({
            'arm': 'ray-named-actor',
            'logical_bytes': n_bytes,
            'actor_init_ms': round(actor_init_ms, 3),
            'topology': 'dynamic_named_actor',
            'ray_inline_threshold_bytes': 100 * KIB,
            'client_peak_rss_mib': peak_rss_mib(),
            **summarize_ms(samples_ms, payload_bytes=n_bytes, bidirectional=True),
        })

        # 2. Ray Compiled Graphs (static compiled topology, single-in-flight)
        ctx = DAGContext.get_current()
        t_comp0 = time.perf_counter()
        with InputNode() as inp:
            dag = actor.exchange.bind(inp)
        compiled = dag.experimental_compile()
        compile_ms = (time.perf_counter() - t_comp0) * 1e3
        cg_ms: list[float] = []
        try:
            for i in range(warmup + iters):
                seq = i + 1
                arr = stamp_field_array(base, seq)
                t0 = time.perf_counter()
                r_seq, r_arr = ray.get(compiled.execute((seq, arr)))
                verify_field_result(r_seq, seq + 1, int(r_arr.size), int(base.size), float(r_arr.sum()))
                del r_arr
                dt_ms = (time.perf_counter() - t0) * 1e3
                if i >= warmup:
                    cg_ms.append(dt_ms)
        finally:
            compiled.teardown()
        out_rows.append({
            'arm': 'ray-compiled-graph',
            'logical_bytes': n_bytes,
            'graph_compile_ms': round(compile_ms, 3),
            'topology': 'static_compiled_dag',
            'cg_initial_buffer_bytes': int(ctx.buffer_size_bytes),
            'cg_max_inflight_executions': int(ctx.max_inflight_executions),
            'client_peak_rss_mib': peak_rss_mib(),
            **summarize_ms(cg_ms, payload_bytes=n_bytes, bidirectional=True),
        })
    finally:
        ray.kill(actor)
    return out_rows


def _run_ray_field_arms(*, n_bytes: int, mode: str, warmup: int, iters: int) -> list[dict[str, Any]]:
    ray = _ensure_ray_started()
    from ray.dag import InputNode  # noqa: PLC0415
    from ray.dag.context import DAGContext  # noqa: PLC0415

    @ray.remote
    class RayFieldActor:
        def __init__(self, size: int) -> None:
            self._base = make_field_array(size)
            self._published = (1, stamp_field_array(self._base, 1))

        def snapshot_published(self, _seq: int) -> tuple[int, np.ndarray]:
            return self._published

        def snapshot_fresh(self, seq: int) -> tuple[int, np.ndarray]:
            return (seq, stamp_field_array(self._base, seq))

    ns = f'bench-field-{os.getpid()}'
    actor_name = f'field-{n_bytes}-{mode}-{time.time_ns()}'
    t_init0 = time.perf_counter()
    _ = RayFieldActor.options(name=actor_name, namespace=ns, lifetime='detached').remote(n_bytes)
    actor = ray.get_actor(actor_name, namespace=ns)
    count_elems = n_bytes // 8
    method = actor.snapshot_published if mode == 'published' else actor.snapshot_fresh
    ray.get(method.remote(1))
    actor_init_ms = (time.perf_counter() - t_init0) * 1e3

    out_rows: list[dict[str, Any]] = []
    try:
        # 1. Detached named actor
        samples_ms: list[float] = []
        er_samples: list[float] = []
        for i in range(warmup + iters):
            exp_seq = 1 if mode == 'published' else (i + 1)
            t0 = time.perf_counter()
            r_seq, r_arr = ray.get(method.remote(exp_seq))
            t_er0 = time.perf_counter()
            total = float(r_arr.sum())
            er_ms = (time.perf_counter() - t_er0) * 1e3
            verify_field_result(r_seq, exp_seq, int(r_arr.size), count_elems, total)
            del r_arr
            dt_ms = (time.perf_counter() - t0) * 1e3
            if i >= warmup:
                samples_ms.append(dt_ms)
                er_samples.append(er_ms)
        out_rows.append({
            'arm': 'ray-named-actor',
            'frame_mode': mode,
            'logical_bytes': n_bytes,
            'actor_init_ms': round(actor_init_ms, 3),
            'topology': 'dynamic_named_actor',
            'extract_and_reduce_p50_ms': round(nearest_rank(er_samples, 0.50), 4),
            'client_peak_rss_mib': peak_rss_mib(),
            **summarize_ms(samples_ms, payload_bytes=n_bytes, bidirectional=False),
        })

        # 2. Ray Compiled Graphs (static compiled topology, release ndarray each iter)
        ctx = DAGContext.get_current()
        t_comp0 = time.perf_counter()
        with InputNode() as inp:
            dag = method.bind(inp)
        compiled = dag.experimental_compile()
        compile_ms = (time.perf_counter() - t_comp0) * 1e3
        cg_ms: list[float] = []
        cg_er: list[float] = []
        try:
            for i in range(warmup + iters):
                exp_seq = 1 if mode == 'published' else (i + 1)
                t0 = time.perf_counter()
                r_seq, r_arr = ray.get(compiled.execute(exp_seq))
                t_er0 = time.perf_counter()
                total = float(r_arr.sum())
                er_ms = (time.perf_counter() - t_er0) * 1e3
                verify_field_result(r_seq, exp_seq, int(r_arr.size), count_elems, total)
                del r_arr
                dt_ms = (time.perf_counter() - t0) * 1e3
                if i >= warmup:
                    cg_ms.append(dt_ms)
                    cg_er.append(er_ms)
        finally:
            compiled.teardown()
        out_rows.append({
            'arm': 'ray-compiled-graph',
            'frame_mode': mode,
            'logical_bytes': n_bytes,
            'graph_compile_ms': round(compile_ms, 3),
            'topology': 'static_compiled_dag',
            'cg_initial_buffer_bytes': int(ctx.buffer_size_bytes),
            'cg_max_inflight_executions': int(ctx.max_inflight_executions),
            'extract_and_reduce_p50_ms': round(nearest_rank(cg_er, 0.50), 4),
            'client_peak_rss_mib': peak_rss_mib(),
            **summarize_ms(cg_ms, payload_bytes=n_bytes, bidirectional=False),
        })
    finally:
        ray.kill(actor)
    return out_rows


def _run_ray_kostya_arm(n: int, warmup: int, iters: int) -> dict[str, Any]:
    ray = _ensure_ray_started()

    @ray.remote
    class RayKostyaActor:
        def __init__(self, count: int) -> None:
            self._data = build_kostya_numpy_dict(count)

        def coords(self) -> dict[str, Any]:
            return self._data

    ns = f'bench-kostya-{os.getpid()}'
    actor_name = f'kostya-{n}-{time.time_ns()}'
    _ = RayKostyaActor.options(name=actor_name, namespace=ns, lifetime='detached').remote(n)
    actor = ray.get_actor(actor_name, namespace=ns)
    rpc_ms: list[float] = []
    extract_ms: list[float] = []
    reduce_ms: list[float] = []
    total_ms: list[float] = []
    try:
        for i in range(warmup + iters):
            t0 = time.perf_counter()
            res = ray.get(actor.coords.remote())
            t1 = time.perf_counter()
            _, ex_ms, red_ms = reduce_kostya_arrays(res, n)
            del res
            t2 = time.perf_counter()
            if i >= warmup:
                rpc_ms.append((t1 - t0) * 1e3)
                extract_ms.append(ex_ms)
                reduce_ms.append(red_ms)
                total_ms.append((t2 - t0) * 1e3)
    finally:
        ray.kill(actor)
    logical_bytes = n * (4 + 8 * 3 + 2)
    return {
        'arm': 'ray-named-actor-arrays',
        'rows': n,
        'logical_numeric_bytes': logical_bytes,
        'encoded_bytes': None,
        'rpc_p50_ms': round(nearest_rank(rpc_ms, 0.50), 4),
        'extract_p50_ms': round(nearest_rank(extract_ms, 0.50), 4),
        'reduce_p50_ms': round(nearest_rank(reduce_ms, 0.50), 4),
        'client_peak_rss_mib': peak_rss_mib(),
        **summarize_ms(total_ms, payload_bytes=logical_bytes, bidirectional=False),
    }


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def run_benchmark(
    *,
    suite: str,
    smoke: bool = False,
    include_ray: bool = False,
    c3_bin: str | None = None,
) -> dict[str, Any]:
    os.environ['C2_ENV_FILE'] = ''
    os.environ['C2_RELAY_ANCHOR_ADDRESS'] = ''
    resolved_c3 = c3_bin or shutil.which('c3')
    selected = ['stages', 'coupling', 'field', 'fanout', 'pubsub_fanout', 'kostya'] if suite == 'all' else [suite]
    results: dict[str, Any] = {}
    try:
        for name in selected:
            if name == 'stages':
                results['stages'] = run_stages_suite(smoke=smoke)
            elif name == 'coupling':
                results['coupling'] = run_coupling_suite(smoke=smoke, include_ray=include_ray)
            elif name == 'field':
                results['field'] = run_field_suite(smoke=smoke, include_ray=include_ray, c3_bin=resolved_c3)
            elif name == 'fanout':
                results['fanout'] = run_fanout_suite(smoke=smoke, c3_bin=resolved_c3)
            elif name == 'pubsub_fanout':
                results['pubsub_fanout'] = run_pubsub_fanout_suite(smoke=smoke, c3_bin=resolved_c3)
            elif name == 'kostya':
                results['kostya'] = run_kostya_suite(smoke=smoke, include_ray=include_ray)
            else:
                raise ValueError(f'unknown suite: {name!r}')
    finally:
        _shutdown_ray_if_running()

    return {
        'schema': SCHEMA_VERSION,
        'timestamp_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
        'smoke': smoke,
        'include_ray': include_ray,
        'environment': collect_environment(c3_bin=resolved_c3, include_ray=include_ray),
        'suites': results,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description='Reproducible FastDB Payload, server stream, and topic benchmark.')
    parser.add_argument(
        '--suite',
        choices=('stages', 'coupling', 'field', 'fanout', 'pubsub_fanout', 'kostya', 'all'),
        default='all',
    )
    parser.add_argument('--smoke', action='store_true', help='Run small fast sizes for verification.')
    parser.add_argument('--include-ray', action='store_true', help='Include Ray detached actor and Compiled Graphs arms.')
    parser.add_argument('--c3-bin', default=None, help='Path to c3 binary for relay arms (defaults to PATH lookup).')
    parser.add_argument('--output', type=Path, default=None, help='Write JSON receipt to this file.')
    args = parser.parse_args(argv)

    receipt = run_benchmark(
        suite=args.suite,
        smoke=args.smoke,
        include_ray=args.include_ray,
        c3_bin=args.c3_bin,
    )
    rendered = json.dumps(receipt, indent=2, sort_keys=False)
    if args.output is not None:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered + '\n', encoding='utf-8')
        print(f'Wrote benchmark receipt to {args.output}')
    else:
        print(rendered)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
