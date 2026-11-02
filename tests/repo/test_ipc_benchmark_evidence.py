"""Evidence format and OS helper contracts without starting the IPC runtime."""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace

import pytest


EVIDENCE = Path(__file__).resolve().parents[2] / 'tools' / 'benchmarks' / 'ipc_evidence.py'


@pytest.fixture
def evidence():
    spec = importlib.util.spec_from_file_location('_ipc_benchmark_evidence_test', EVIDENCE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_raw_attempts_and_timeline_include_identity_and_provenance(evidence, tmp_path):
    identity = {'role': 'normal', 'pid': 123, 'worker_index': 2}
    config = {'payload_bytes': 32, 'mode': 'direct'}
    provenance = {'commit': 'abc', 'native_sha256': 'def'}
    with evidence.SampleLog(tmp_path, identity, config, provenance) as log:
        success = log.sample('r1', 100, 120, 'success', integrity_ok=True)
        failure = log.sample('r2', 125, 160, 'error', 'ValueError', 'bad bytes')
        event = log.event('paused', marker='copy_start')
        assert len(log.samples) == 2
        assert log.samples == [success, failure]
        assert log.samples_path.is_file()
        assert log.events_path.is_file()

    samples = [json.loads(line) for line in log.samples_path.read_text(encoding='utf-8').splitlines()]
    events = [json.loads(line) for line in log.events_path.read_text(encoding='utf-8').splitlines()]
    assert samples[0]['kind'] == 'run'
    assert samples[0]['config'] == config
    assert samples[0]['provenance'] == provenance
    assert samples[1]['identity'] == identity
    assert samples[1]['latency_ns'] == 20
    assert samples[1]['measurement'] is True
    assert samples[1]['integrity_ok'] is True
    assert samples[2]['status'] == 'error'
    assert samples[2]['error_type'] == 'ValueError'
    assert events == [event]
    assert event['monotonic_ns'] > 0


def test_summary_counts_errors_and_timeouts_in_quantiles_and_denominator(evidence, tmp_path):
    with evidence.SampleLog(tmp_path, {'role': 'normal', 'pid': 1, 'worker_index': 0}, {}, {}) as log:
        log.sample('warmup', 0, 1000, 'success', phase='warmup', measurement=False)
        log.sample('a', 0, 10, 'success')
        log.sample('b', 10, 30, 'error', 'BrokenPipeError', 'closed')
        log.sample('c', 30, 60, 'timeout', 'TimeoutError', 'deadline')
        log.sample('d', 60, 160, 'success')
    summary = evidence.summarize_samples(log.samples)
    assert summary['attempts'] == 4
    assert summary['successes'] == 2
    assert summary['failures'] == 1
    assert summary['timeouts'] == 1
    assert summary['unsuccessful'] == 2
    assert summary['failure_rate'] == 0.5
    assert (summary['p50_ns'], summary['p95_ns'], summary['p99_ns']) == (20, 100, 100)
    assert summary['elapsed_ns'] == 160
    assert summary['throughput_calls_per_s'] == 2 * 1_000_000_000 / 160
    assert 'errors and timeouts' in summary['quantile_population']
    assert 'ceil(p*n)-1' in summary['quantile_method']
    assert evidence.summarize_samples(log.samples, phase='warmup')['attempts'] == 0
    empty = evidence.summarize_samples([])
    assert empty['attempts'] == 0
    assert empty['p99_ns'] is None
    assert empty['failure_rate'] is None
    assert empty['throughput_calls_per_s'] is None


def test_each_sample_is_flushed_before_abrupt_process_exit(tmp_path):
    script = '''
import importlib.util, os
from pathlib import Path
spec = importlib.util.spec_from_file_location("ipc_evidence", __import__("sys").argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
log = module.SampleLog(Path(__import__("sys").argv[2]), {"role": "crash", "pid": os.getpid(), "worker_index": 0}, {}, {})
log.sample("last", 1, 4, "timeout")
os._exit(7)
'''
    result = subprocess.run(
        [sys.executable, '-c', script, str(EVIDENCE), str(tmp_path)],
        capture_output=True, text=True, timeout=10,
    )
    assert result.returncode == 7
    path = next(tmp_path.glob('crash-0-*.samples.jsonl'))
    rows = [json.loads(line) for line in path.read_text(encoding='utf-8').splitlines()]
    assert [row['kind'] for row in rows] == ['run', 'sample']
    assert rows[1]['status'] == 'timeout'
    assert rows[1]['latency_ns'] == 3


def test_unavailable_os_fields_are_null_with_reasons(evidence):
    foreign = evidence.process_snapshot(os.getpid() + 100000)
    assert set(foreign) == {
        'current_rss_bytes', 'peak_rss_bytes', 'fd_count',
        'handle_count', 'mapping_count',
        'shared_mapping_count', 'shared_mappings',
    }
    assert all(item['value'] is None and item['reason'] for item in foreign.values())
    current = evidence.process_snapshot()
    assert all(set(item) == {'value', 'reason'} for item in current.values())
    if sys.platform == 'darwin':
        assert current['mapping_count']['value'] is None
        assert current['mapping_count']['reason']
    assert all(not probe['available'] and probe['reason'] for probe in evidence.INTERNAL_PROBES.values())


def test_linux_snapshot_keeps_every_metric_in_the_same_shape(evidence, monkeypatch):
    maps = ('1000-2000 rw-p 00000000 00:00 0\n'
            '2000-3000 rw-s 00000000 00:1a 1 /dev/shm/c2 (deleted)\n'
            '3000-4000 rw-s 00000000 00:1b 2 /tmp/c2-spill (deleted)\n')
    monkeypatch.setattr(Path, 'read_text', lambda path, **kwargs:
                        maps if path.name == 'maps' else 'VmRSS: 100 kB\nVmHWM: 200 kB\n')
    monkeypatch.setattr(evidence.os, 'listdir', lambda path: ['0', '1', '2'])
    result = evidence._linux_process_snapshot(123)
    assert all(set(value) == {'value', 'reason'} for value in result.values())
    assert result['mapping_count']['value'] == 3
    assert result['shared_mapping_count']['value'] == 2
    assert len(result['shared_mappings']['value']) == 2


def test_evidence_records_the_high_resolution_clock(evidence, monkeypatch, tmp_path):
    monkeypatch.setattr(evidence.time, 'monotonic_ns', lambda: 0)
    monkeypatch.setattr(evidence.time, 'perf_counter_ns', lambda: 1234567)
    with evidence.SampleLog(tmp_path, {'role': 'normal', 'pid': 1, 'worker_index': 0}, {}, {}) as log:
        event = log.event('ready')
    metadata = json.loads(log.samples_path.read_text(encoding='utf-8').splitlines()[0])
    assert metadata['clock']['name'] == 'perf_counter_ns'
    assert event['monotonic_ns'] == 1234567


@pytest.mark.parametrize('winerror', [None, 5, 32])
def test_marker_read_waits_for_windows_sharing_windows(evidence, monkeypatch, tmp_path, winerror):
    path = tmp_path / 'ready.json'
    path.write_text('{"pid": 1234}')
    read = Path.read_text
    calls = 0
    def sharing_then_ready(target, **kwargs):
        nonlocal calls
        calls += 1
        if calls == 1:
            # io.open surfaces Windows sharing windows as errno 13 through the
            # CRT _wopen mapping; the winerror attribute carries no usable
            # value and must not gate the retry.
            error = PermissionError(13, 'Permission denied')
            error.winerror = winerror
            raise error
        return read(target, **kwargs)
    monkeypatch.setattr(evidence.sys, 'platform', 'win32')
    monkeypatch.setattr(Path, 'read_text', sharing_then_ready)
    assert evidence.read_marker(path) is None
    assert evidence.read_marker(path) == {'pid': 1234}


def test_marker_read_reraises_permission_errors_off_windows(evidence, monkeypatch, tmp_path):
    path = tmp_path / 'ready.json'
    path.write_text('{"pid": 1234}')
    def denied(target, **kwargs):
        raise PermissionError(13, 'Permission denied')
    monkeypatch.setattr(evidence.sys, 'platform', 'linux')
    monkeypatch.setattr(Path, 'read_text', denied)
    with pytest.raises(PermissionError):
        evidence.read_marker(path)


def test_marker_publish_retries_windows_replace_sharing(evidence, monkeypatch, tmp_path):
    path = tmp_path / 'released.json'
    replace = Path.replace
    calls = 0
    def sharing_then_done(target, destination):
        nonlocal calls
        calls += 1
        if calls <= 2:
            raise PermissionError(13, 'Permission denied')
        return replace(target, destination)
    monkeypatch.setattr(evidence.sys, 'platform', 'win32')
    monkeypatch.setattr(evidence.time, 'sleep', lambda seconds: None)
    monkeypatch.setattr(Path, 'replace', sharing_then_done)
    evidence.publish_marker(path, {'released_ns': 123})
    assert calls == 3
    assert evidence.read_marker(path) == {'released_ns': 123}


def test_marker_publish_reraises_sharing_off_windows(evidence, monkeypatch, tmp_path):
    path = tmp_path / 'released.json'
    def denied(target, destination):
        raise PermissionError(13, 'Permission denied')
    monkeypatch.setattr(evidence.sys, 'platform', 'linux')
    monkeypatch.setattr(Path, 'replace', denied)
    with pytest.raises(PermissionError):
        evidence.publish_marker(path, {'released_ns': 123})


def test_marker_publish_keeps_the_original_fact(evidence, tmp_path):
    path = tmp_path / 'released.json'
    evidence.publish_marker(path, {'released_ns': 123})
    evidence.publish_marker(path, {'released_ns': 456})
    assert evidence.read_marker(path) == {'released_ns': 123}


class FakeFunction:
    def __init__(self, name, result, calls):
        self.__name__ = name
        self.result = result
        self.calls = calls

    def __call__(self, *args):
        self.calls.append((self.__name__, args))
        return self.result


def test_windows_suspend_resume_uses_native_calls_and_closes_handles(evidence, monkeypatch):
    calls = []
    kernel32 = SimpleNamespace(
        OpenProcess=FakeFunction('OpenProcess', 99, calls),
        CloseHandle=FakeFunction('CloseHandle', 1, calls),
    )
    ntdll = SimpleNamespace(
        NtSuspendProcess=FakeFunction('NtSuspendProcess', 0, calls),
        NtResumeProcess=FakeFunction('NtResumeProcess', 0, calls),
    )
    monkeypatch.setattr(evidence.sys, 'platform', 'win32')
    monkeypatch.setattr(
        evidence.ctypes, 'WinDLL',
        lambda name, **_kwargs: kernel32 if name == 'kernel32' else ntdll,
        raising=False,
    )
    evidence.suspend_process(1234)
    evidence.resume_process(1234)
    assert [name for name, _args in calls] == [
        'OpenProcess', 'NtSuspendProcess', 'CloseHandle',
        'OpenProcess', 'NtResumeProcess', 'CloseHandle',
    ]
    assert calls[0][1] == (0x0800, False, 1234)
    assert calls[1][1] == (99,)


def test_windows_native_failure_still_closes_handle(evidence, monkeypatch):
    calls = []
    kernel32 = SimpleNamespace(
        OpenProcess=FakeFunction('OpenProcess', 42, calls),
        CloseHandle=FakeFunction('CloseHandle', 1, calls),
    )
    ntdll = SimpleNamespace(
        NtSuspendProcess=FakeFunction('NtSuspendProcess', -1, calls),
        NtResumeProcess=FakeFunction('NtResumeProcess', 0, calls),
    )
    monkeypatch.setattr(evidence.ctypes, 'WinDLL', lambda name, **_kwargs: kernel32 if name == 'kernel32' else ntdll, raising=False)
    with pytest.raises(OSError, match='NTSTATUS'):
        evidence._windows_change_process_state(1234, resume=False)
    assert [name for name, _args in calls][-1] == 'CloseHandle'


@pytest.mark.skipif(os.name == 'nt', reason='SIGSTOP/SIGCONT are Unix-only signals')
def test_unix_suspend_resume_signals_only_given_pid(evidence, monkeypatch):
    calls = []
    monkeypatch.setattr(evidence.sys, 'platform', 'linux')
    monkeypatch.setattr(evidence.os, 'kill', lambda pid, sig: calls.append((pid, sig)))
    evidence.suspend_process(1234)
    evidence.resume_process(1234)
    assert calls == [
        (1234, evidence.signal.SIGSTOP),
        (1234, evidence.signal.SIGCONT),
    ]


@pytest.mark.parametrize('state', ['timeout', 'descendants', 'clean'])
def test_owned_run_closes_process_tree_on_every_exit(evidence, monkeypatch, state):
    trace = []
    def wait(timeout):
        trace.append(('wait', timeout))
        if state == 'timeout':
            raise subprocess.TimeoutExpired(['fixture'], timeout)
        return 0
    tree = SimpleNamespace(
        process=SimpleNamespace(wait=wait),
        alive=lambda: state == 'descendants',
        diagnostics=lambda: [{'owned': 123}],
        close=lambda: trace.append('closed'),
    )
    monkeypatch.setattr(evidence, 'platform_process_support', lambda: SimpleNamespace(ProcessTree=lambda *args: tree))
    if state == 'timeout':
        with pytest.raises(subprocess.TimeoutExpired):
            evidence.run_owned(['fixture'], cwd='.', env={}, stdout=None, timeout=1)
    elif state == 'descendants':
        with pytest.raises(OSError, match='live owned descendants'):
            evidence.run_owned(['fixture'], cwd='.', env={}, stdout=None, timeout=1)
    else:
        assert evidence.run_owned(['fixture'], cwd='.', env={}, stdout=None, timeout=1).returncode == 0
    assert trace == [('wait', 1), 'closed']
