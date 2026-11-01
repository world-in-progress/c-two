"""Raw, process-local evidence for direct IPC benchmarks.

This module uses only the standard library and does not import the runtime.
Each attempt is flushed before the caller proceeds, so an interrupted run
retains its completed attempts and event timeline.
"""

from __future__ import annotations

import ctypes
import importlib.util
import json
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time


_UNMEASURED = {'available': False, 'reason': '未测量（无内部探针）'}
INTERNAL_PROBES = {
    'inflight_loans': dict(_UNMEASURED),
    'release_backlog': dict(_UNMEASURED),
    'release_queue_to_settlement_ns': dict(_UNMEASURED),
}
_QUANTILE_METHOD = 'nearest-rank: sorted[ceil(p*n)-1]'
_QUANTILE_POPULATION = 'all measurement attempts, including errors and timeouts'


def _write_json_line(stream, record: dict) -> None:
    stream.write(json.dumps(record, ensure_ascii=False, sort_keys=True) + '\n')
    stream.flush()


class SampleLog:
    """Write one process's attempts and timeline to separate JSONL files."""

    def __init__(
        self, output_dir: Path, identity: dict, config: dict, provenance: dict,
    ) -> None:
        self.identity = {key: identity[key] for key in ('role', 'pid', 'worker_index')}
        role = str(self.identity['role'])
        if not role or any(char not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-' for char in role):
            raise ValueError('role must be a nonempty filename-safe identifier')
        output_dir = Path(output_dir)
        output_dir.mkdir(parents=True, exist_ok=True)
        stem = f"{role}-{self.identity['worker_index']}-{self.identity['pid']}"
        self.samples_path = output_dir / f'{stem}.samples.jsonl'
        self.events_path = output_dir / f'{stem}.events.jsonl'
        self._samples_file = self.samples_path.open('w', encoding='utf-8')
        self._events_file = self.events_path.open('w', encoding='utf-8')
        self.samples: list[dict] = []
        self._lock = threading.Lock()
        _write_json_line(self._samples_file, {
            'kind': 'run', 'identity': self.identity,
            'config': config, 'provenance': provenance,
            'monotonic_ns': time.monotonic_ns(),
        })

    def sample(
        self, request_id, start_ns: int, end_ns: int, status: str,
        error_type: str | None = None, error_message: str | None = None,
        phase: str = 'measurement', integrity_ok: bool | None = None,
        measurement: bool = True,
    ) -> dict:
        if status not in ('success', 'error', 'timeout'):
            raise ValueError(f'unsupported attempt status: {status}')
        if end_ns < start_ns:
            raise ValueError('end_ns precedes start_ns')
        record = {
            'kind': 'sample', 'identity': self.identity,
            'request_id': request_id, 'start_ns': start_ns, 'end_ns': end_ns,
            'latency_ns': end_ns - start_ns, 'status': status,
            'error_type': error_type, 'error_message': error_message,
            'phase': phase, 'measurement': measurement,
            'integrity_ok': integrity_ok,
        }
        with self._lock:
            _write_json_line(self._samples_file, record)
            self.samples.append(record)
        return record

    def event(self, event: str, **details) -> dict:
        record = {
            **details, 'kind': 'event', 'identity': self.identity,
            'event': event, 'monotonic_ns': time.monotonic_ns(),
        }
        with self._lock:
            _write_json_line(self._events_file, record)
        return record

    def close(self) -> None:
        self._samples_file.close()
        self._events_file.close()

    def __enter__(self) -> SampleLog:
        return self

    def __exit__(self, *_exc) -> None:
        self.close()


def summarize_samples(samples: list[dict], phase: str | None = None) -> dict:
    selected = [
        sample for sample in samples
        if sample.get('measurement') is True
        and (phase is None or sample.get('phase') == phase)
    ]
    attempts = len(selected)
    successes = sum(sample['status'] == 'success' for sample in selected)
    timeouts = sum(sample['status'] == 'timeout' for sample in selected)
    failures = attempts - successes - timeouts
    latencies = sorted(sample['latency_ns'] for sample in selected)

    def quantile(fraction: float) -> int | None:
        return latencies[math.ceil(fraction * attempts) - 1] if attempts else None

    elapsed_ns = (
        max(sample['end_ns'] for sample in selected)
        - min(sample['start_ns'] for sample in selected)
        if attempts else None
    )
    return {
        'attempts': attempts, 'successes': successes, 'failures': failures,
        'timeouts': timeouts, 'unsuccessful': failures + timeouts,
        'failure_rate': (failures + timeouts) / attempts if attempts else None,
        'p50_ns': quantile(0.50), 'p95_ns': quantile(0.95),
        'p99_ns': quantile(0.99),
        'throughput_calls_per_s': (
            successes * 1_000_000_000 / elapsed_ns if elapsed_ns else None
        ),
        'elapsed_ns': elapsed_ns,
        'quantile_method': _QUANTILE_METHOD,
        'quantile_population': _QUANTILE_POPULATION,
    }


def _available(value: int) -> dict:
    return {'value': value, 'reason': None}


def _unavailable(reason: str) -> dict:
    return {'value': None, 'reason': reason}


def _empty_snapshot(reason: str) -> dict:
    return {key: _unavailable(reason) for key in (
        'current_rss_bytes', 'peak_rss_bytes', 'fd_count',
        'handle_count', 'mapping_count',
    )}


def _linux_process_snapshot(pid: int) -> dict:
    result = _empty_snapshot('not provided by this platform')
    proc = Path('/proc') / str(pid)
    try:
        status = (proc / 'status').read_text(encoding='utf-8')
        for line in status.splitlines():
            if line.startswith('VmRSS:'):
                result['current_rss_bytes'] = _available(int(line.split()[1]) * 1024)
            elif line.startswith('VmHWM:'):
                result['peak_rss_bytes'] = _available(int(line.split()[1]) * 1024)
    except (OSError, ValueError, IndexError) as exc:
        reason = f'/proc status unavailable: {type(exc).__name__}'
        result['current_rss_bytes'] = _unavailable(reason)
        result['peak_rss_bytes'] = _unavailable(reason)
    try:
        result['fd_count'] = _available(len(os.listdir(proc / 'fd')))
    except OSError as exc:
        result['fd_count'] = _unavailable(f'/proc fd unavailable: {type(exc).__name__}')
    try:
        with (proc / 'maps').open(encoding='utf-8') as stream:
            result['mapping_count'] = _available(sum(1 for _ in stream))
    except OSError as exc:
        result['mapping_count'] = _unavailable(f'/proc maps unavailable: {type(exc).__name__}')
    return result


def _macos_process_snapshot(pid: int) -> dict:
    result = _empty_snapshot('not provided by this platform')
    try:
        completed = subprocess.run(
            ['ps', '-o', 'rss=', '-p', str(pid)], capture_output=True,
            text=True, timeout=2, check=True,
        )
        result['current_rss_bytes'] = _available(int(completed.stdout.strip()) * 1024)
    except (OSError, ValueError, subprocess.SubprocessError) as exc:
        result['current_rss_bytes'] = _unavailable(f'ps RSS unavailable: {type(exc).__name__}')
    if pid == os.getpid():
        try:
            import resource
            result['peak_rss_bytes'] = _available(int(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss))
        except (ImportError, OSError, ValueError) as exc:
            result['peak_rss_bytes'] = _unavailable(f'peak RSS unavailable: {type(exc).__name__}')
        try:
            result['fd_count'] = _available(len(os.listdir('/dev/fd')))
        except OSError as exc:
            result['fd_count'] = _unavailable(f'fd count unavailable: {type(exc).__name__}')
    else:
        result['peak_rss_bytes'] = _unavailable('macOS foreign peak RSS unavailable')
        result['fd_count'] = _unavailable('macOS foreign fd count unavailable')
    result['mapping_count'] = _unavailable('mapping count not sampled on macOS')
    return result


def _windows_process_snapshot(pid: int) -> dict:
    from ctypes import wintypes

    class ProcessMemoryCounters(ctypes.Structure):
        _fields_ = [
            ('cb', wintypes.DWORD), ('PageFaultCount', wintypes.DWORD),
            ('PeakWorkingSetSize', ctypes.c_size_t),
            ('WorkingSetSize', ctypes.c_size_t),
            ('QuotaPeakPagedPoolUsage', ctypes.c_size_t),
            ('QuotaPagedPoolUsage', ctypes.c_size_t),
            ('QuotaPeakNonPagedPoolUsage', ctypes.c_size_t),
            ('QuotaNonPagedPoolUsage', ctypes.c_size_t),
            ('PagefileUsage', ctypes.c_size_t),
            ('PeakPagefileUsage', ctypes.c_size_t),
        ]

    result = _empty_snapshot('not provided by this platform')
    try:
        kernel32 = ctypes.WinDLL('kernel32', use_last_error=True)
        psapi = ctypes.WinDLL('psapi', use_last_error=True)
        kernel32.GetCurrentProcess.restype = wintypes.HANDLE
        psapi.GetProcessMemoryInfo.argtypes = (
            wintypes.HANDLE, ctypes.POINTER(ProcessMemoryCounters), wintypes.DWORD,
        )
        psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
        kernel32.GetProcessHandleCount.argtypes = (
            wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD),
        )
        kernel32.GetProcessHandleCount.restype = wintypes.BOOL
        foreign = pid != os.getpid()
        kernel32.OpenProcess.argtypes = (wintypes.DWORD, wintypes.BOOL, wintypes.DWORD)
        kernel32.OpenProcess.restype = wintypes.HANDLE
        kernel32.CloseHandle.argtypes = (wintypes.HANDLE,)
        kernel32.CloseHandle.restype = wintypes.BOOL
        handle = kernel32.OpenProcess(0x0410, False, pid) if foreign else kernel32.GetCurrentProcess()
        if not handle:
            return _empty_snapshot(f'OpenProcess counters failed: {ctypes.get_last_error()}')
        counters = ProcessMemoryCounters()
        counters.cb = ctypes.sizeof(counters)
        if psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
            result['current_rss_bytes'] = _available(int(counters.WorkingSetSize))
            result['peak_rss_bytes'] = _available(int(counters.PeakWorkingSetSize))
        else:
            reason = f'GetProcessMemoryInfo failed: {ctypes.get_last_error()}'
            result['current_rss_bytes'] = _unavailable(reason)
            result['peak_rss_bytes'] = _unavailable(reason)
        count = wintypes.DWORD()
        if kernel32.GetProcessHandleCount(handle, ctypes.byref(count)):
            result['handle_count'] = _available(int(count.value))
        else:
            result['handle_count'] = _unavailable(
                f'GetProcessHandleCount failed: {ctypes.get_last_error()}'
            )
        if foreign and not kernel32.CloseHandle(handle):
            raise OSError(ctypes.get_last_error(), 'CloseHandle counter process')
    except (AttributeError, OSError) as exc:
        return _empty_snapshot(f'Windows process counters unavailable: {type(exc).__name__}')
    return result


def process_snapshot(pid: int | None = None) -> dict:
    """Sample OS counters; unsupported fields remain explicit nulls."""
    if pid is None:
        pid = os.getpid()
    if sys.platform.startswith('linux'):
        return _linux_process_snapshot(pid)
    if sys.platform == 'darwin':
        return _macos_process_snapshot(pid)
    if sys.platform == 'win32':
        return _windows_process_snapshot(pid)
    return _empty_snapshot(f'unsupported platform: {sys.platform}')


def _windows_change_process_state(pid: int, resume: bool) -> None:
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL('kernel32', use_last_error=True)
    ntdll = ctypes.WinDLL('ntdll', use_last_error=True)
    kernel32.OpenProcess.argtypes = (wintypes.DWORD, wintypes.BOOL, wintypes.DWORD)
    kernel32.OpenProcess.restype = wintypes.HANDLE
    kernel32.CloseHandle.argtypes = (wintypes.HANDLE,)
    kernel32.CloseHandle.restype = wintypes.BOOL
    operation = ntdll.NtResumeProcess if resume else ntdll.NtSuspendProcess
    operation.argtypes = (wintypes.HANDLE,)
    operation.restype = ctypes.c_long
    handle = kernel32.OpenProcess(0x0800, False, pid)  # PROCESS_SUSPEND_RESUME
    if not handle:
        raise OSError(ctypes.get_last_error(), f'OpenProcess failed for pid {pid}')
    try:
        status = operation(handle)
        if status != 0:
            raise OSError(f'{operation.__name__} failed with NTSTATUS 0x{status & 0xffffffff:08x}')
    finally:
        if not kernel32.CloseHandle(handle):
            raise OSError(ctypes.get_last_error(), f'CloseHandle failed for pid {pid}')


def suspend_process(proc: subprocess.Popen) -> None:
    """Stop a process spawned by the caller until resume_process is called."""
    if sys.platform == 'win32':
        _windows_change_process_state(proc.pid, resume=False)
    else:
        os.kill(proc.pid, signal.SIGSTOP)


def resume_process(proc: subprocess.Popen) -> None:
    """Continue a previously stopped process spawned by the caller."""
    if sys.platform == 'win32':
        _windows_change_process_state(proc.pid, resume=True)
    else:
        os.kill(proc.pid, signal.SIGCONT)


def platform_process_support():
    """Reuse the repository's owned groups/Jobs, without resource leases."""
    name = '_c2_benchmark_process_support'
    if name not in sys.modules:
        path = Path(__file__).resolve().parents[1] / 'dev' / 'test_platform.py'
        spec = importlib.util.spec_from_file_location(name, path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        sys.modules[name] = module
    return sys.modules[name]


def run_owned(command, *, cwd, env, stdout, timeout):
    """Bound a controller and its descendants on Unix and Windows alike."""
    tree = platform_process_support().ProcessTree(command, cwd, env, stdout)
    try:
        code = tree.process.wait(timeout=timeout)
        if tree.alive():
            raise OSError(f'controller exited with live owned descendants: {tree.diagnostics()}')
        return subprocess.CompletedProcess(command, code)
    finally:
        # close waits for observed-empty group/Job, including after timeout.
        tree.close()
