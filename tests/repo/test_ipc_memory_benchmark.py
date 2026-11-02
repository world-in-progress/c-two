"""Fail-closed contracts for the subprocess benchmark's acceptance records."""
from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

ROOT = Path(__file__).resolve().parents[2]
HARNESS = ROOT / 'tools' / 'benchmarks' / 'ipc_memory.py'


@pytest.fixture
def harness():
    name = '_ipc_memory_benchmark_contract_test'
    spec = importlib.util.spec_from_file_location(name, HARNESS)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    try:
        yield module
    finally:
        sys.modules.pop(name, None)


@pytest.mark.parametrize('key,value', [
    ('pool_enabled', 'false'),
    ('memory_stats', 'false'),
    ('require_memory_stats', 1),
    ('allow_unavailable_stats_after_shutdown', 'false'),
    ('expect_error_substring', ''),
    ('expect_error_substring', 1),
    ('child_timeout', '1'),
    ('child_timeout', True),
    ('child_timeout', float('inf')),
    ('python', None),
])
def test_invalid_row_spec_fails_before_any_spawn(harness, monkeypatch, key, value):
    row = dict(harness.matrix_rows()[0], python=sys.executable)
    row[key] = value
    monkeypatch.setattr(sys, 'argv', [str(HARNESS), '--row-spec', json.dumps(row)])

    def unexpected_spawn(*args, **kwargs):
        pytest.fail('invalid row spec reached process creation')

    monkeypatch.setattr(harness.subprocess, 'Popen', unexpected_spawn)
    with pytest.raises(SystemExit) as exc:
        harness.main()
    assert exc.value.code == 2


@pytest.mark.parametrize('case,returncode,expected_exit', [
    ('valid', 0, 0),
    ('valid', 1, 1),
    ('nonobject', 0, 1),
    ('server_hash_mismatch', 0, 1),
])
@pytest.mark.parametrize('use_job', [False, True])
def test_matrix_requires_successful_process_and_complete_identity(
    harness, monkeypatch, tmp_path, case, returncode, expected_exit, use_job,
):
    identity = {'native_path': 'fixture-native.so', 'native_sha256': 'a' * 64}
    record = {
        'ok': True, 'failures': [],
        'benchmark': {'workers': [dict(identity)], 'server': dict(identity)},
    }
    if case == 'nonobject':
        record = []
    elif case == 'server_hash_mismatch':
        record['benchmark']['server']['native_sha256'] = 'b' * 64

    class CompletedRow:
        pid = 999999  # No actual process is created by this fixture.

        def __init__(self, *args, **kwargs):
            assert kwargs['start_new_session'] is (not use_job)
            assert kwargs['creationflags'] == (4 if use_job else 0)
            self.returncode = returncode

        def communicate(self, timeout=None):
            return json.dumps(record), ''

        def poll(self):
            return self.returncode

        def kill(self):
            pytest.fail('a completed row must not be signalled')

    job_events = []
    job = SimpleNamespace(
        assign_and_resume=lambda proc: job_events.append(('assigned', proc)),
        alive=lambda: False,
        close=lambda: job_events.append(('closed', None)),
    )
    monkeypatch.setattr(harness, 'row_job', lambda: job if use_job else None)
    monkeypatch.setattr(harness.subprocess, 'Popen', CompletedRow)
    monkeypatch.setattr(harness, 'source_commit', lambda: 'fixture')
    args = SimpleNamespace(
        output_dir=tmp_path, only='idle_small_rpc_default_buddy', python=sys.executable,
        memory_stats=False, require_memory_stats=False,
        allow_unavailable_stats_after_shutdown=False, child_timeout=2, row_timeout=2,
    )
    assert harness.run_matrix(args) == expected_exit
    result = json.loads((tmp_path / 'matrix.json').read_text())
    assert result['complete'] is False
    assert result['rows'][0]['exit_code'] == returncode
    if use_job:
        assert len(job_events) == 2
        assert job_events[0][0] == 'assigned'
        assert isinstance(job_events[0][1], CompletedRow)
        assert job_events[1] == ('closed', None)
    if returncode != 0:
        assert result['totals']['passed'] == 0
        assert result['totals']['failed'] == 1
    if case == 'server_hash_mismatch':
        assert result['native']['agreement'] is False


def test_barrier_waits_for_every_ready_worker(harness, tmp_path):
    import threading
    import time
    from types import SimpleNamespace
    from ipc_evidence import SampleLog

    coord = tmp_path / 'coord'
    coord.mkdir()
    controller = SampleLog(tmp_path, {'role': 'controller', 'pid': 100, 'worker_index': -1}, {}, {})
    logs = [SampleLog(tmp_path, {'role': 'normal', 'pid': 100, 'worker_index': i}, {}, {}) for i in range(2)]
    children = [SimpleNamespace(poll=lambda: None) for _ in logs]
    observed = []
    waiter = threading.Thread(target=lambda: observed.append(harness.release_workers(coord, children, 2, controller)))
    waiter.start()
    first = threading.Thread(target=lambda: harness.wait_for_release(coord, 0, 2, logs[0]))
    first.start()
    while not (coord / 'ready-0.json').exists():
        time.sleep(0.001)
    assert not (coord / 'release.json').exists()
    second = threading.Thread(target=lambda: harness.wait_for_release(coord, 1, 2, logs[1]))
    second.start()
    for thread in (first, second, waiter):
        thread.join(3)
        assert not thread.is_alive()
    release = json.loads((coord / 'release.json').read_text())
    assert release == observed[0]
    assert all(json.loads((coord / f'ready-{i}.json').read_text())['ready_ns'] <= release['release_ns'] for i in range(2))
    controller.close()
    for log in logs:
        log.close()


def test_failed_worker_prevents_barrier_release(harness, tmp_path):
    from ipc_evidence import SampleLog
    with SampleLog(tmp_path, {'role': 'controller', 'pid': 100, 'worker_index': -1}, {}, {}) as log:
        with pytest.raises(RuntimeError, match='before the ready barrier'):
            harness.release_workers(tmp_path, [SimpleNamespace(poll=lambda: 1)], 1, log)
    assert not (tmp_path / 'release.json').exists()


def test_failed_and_timed_out_calls_are_persisted(harness, tmp_path):
    from ipc_evidence import SampleLog, summarize_samples
    with SampleLog(tmp_path, {'role': 'normal', 'pid': 100, 'worker_index': 0}, {}, {}) as log:
        def timeout(_):
            raise TimeoutError('deadline')
        def error(_):
            raise RuntimeError('failure')
        for index, call in enumerate((lambda value: value, error, timeout, lambda _: b'wrong')):
            harness.call_sample(log, index, call, b'expected')
        summary = summarize_samples(log.samples)
        assert (summary['attempts'], summary['successes'], summary['failures'], summary['timeouts']) == (4, 1, 2, 1)
        assert summary['failure_rate'] == 0.75
        assert [sample['request_id'] for sample in log.samples] == list(range(4))
        assert log.samples[-1]['integrity_ok'] is False
        assert len(log.samples_path.read_text().splitlines()) == 5


def test_memory_stats_use_only_common_current_fields(harness):
    scope = {'role': 'server', 'state': 'active', 'limits': {}, 'cells': {}, 'abandoned_releases': 7}
    holds = {key: 0 for key in ('active_holds', 'total_held_bytes', 'oldest_hold_seconds', 'by_direction')}
    holds['historical_field'] = 99
    stats = harness.common_memory_stats({'runtime_outgoing': None, 'server': scope, 'retired': [scope], 'holds': holds})
    assert set(stats['holds']) == {'active_holds', 'total_held_bytes', 'oldest_hold_seconds', 'by_direction'}
    assert 'abandoned_releases' not in stats['server']
    assert 'abandoned_releases' not in stats['retired'][0]


def test_windows_row_is_assigned_before_resume_and_job_is_closed(harness, monkeypatch, tmp_path):
    trace = []
    native = {'native_path': '/fixture/native', 'native_sha256': 'a' * 64}
    row = {'ok': True, 'failures': [], 'benchmark': {'workers': [native], 'server': native}}
    class Child:
        pid = 424242
        returncode = 0
        def __init__(self, command, **kwargs):
            assert kwargs['creationflags'] == 4
            assert kwargs['start_new_session'] is False
            trace.append('spawn_suspended')
        def communicate(self, timeout=None):
            trace.append('communicate')
            return json.dumps(row), ''
    job = SimpleNamespace(assign_and_resume=lambda proc: trace.append('assign_resume'),
                          alive=lambda: False, close=lambda: trace.append('job_closed'))
    monkeypatch.setattr(harness, 'row_job', lambda: job)
    monkeypatch.setattr(harness.subprocess, 'Popen', Child)
    monkeypatch.setattr(harness, 'source_commit', lambda: 'fixture')
    args = SimpleNamespace(output_dir=tmp_path, only='idle_small_rpc_default_buddy',
        python=sys.executable, memory_stats=False, require_memory_stats=False,
        allow_unavailable_stats_after_shutdown=False, child_timeout=2, row_timeout=2)
    assert harness.run_matrix(args) == 0
    assert trace == ['spawn_suspended', 'assign_resume', 'communicate', 'job_closed']


def test_worker_records_every_call_and_only_readies_after_warmup(harness, monkeypatch, tmp_path):
    from types import ModuleType
    cc = ModuleType('c_two')
    cc._native = object()
    cc.crm = lambda **kwargs: lambda cls: cls
    state = {'calls': 0}
    class Proxy:
        def echo(self, payload):
            index = state['calls']
            state['calls'] += 1
            if index == 3:
                raise TimeoutError('call deadline')
            if index == 4:
                raise RuntimeError('resource error')
            return payload
    proxy = Proxy()
    cc.connect = lambda *args, **kwargs: proxy
    cc.with_call_options = lambda client, **kwargs: client
    cc.hold = lambda call: lambda payload: SimpleNamespace(value=call(payload), release=lambda: None)
    cc.close = lambda client: None
    cc.shutdown = lambda **kwargs: {'completed': True}
    monkeypatch.setitem(sys.modules, 'c_two', cc)
    monkeypatch.setattr(harness, 'configure', lambda cc, args: {})
    monkeypatch.setattr(harness, 'source_commit', lambda: 'fixture')
    monkeypatch.setattr(harness, 'runtime_provenance', lambda native: {
        'module_path': '/fixture/native', 'sha256': 'a' * 64, 'repo': {'head': 'fixture'},
    })
    empty = {'runtime_outgoing': None, 'server': None, 'retired': [], 'holds': {
        'active_holds': 0, 'total_held_bytes': 0, 'oldest_hold_seconds': 0, 'by_direction': {},
    }}
    monkeypatch.setattr(harness, 'snapshot_memory_stats', lambda: {'available': True, 'stats': empty})
    def ready(*args):
        assert state['calls'] == 2
        return {'release_ns': 1}
    monkeypatch.setattr(harness, 'wait_for_release', ready)
    args = harness.row_namespace(dict(harness.matrix_rows()[0], python=sys.executable,
        calls=3, warmup=2, output_dir=str(tmp_path), memory_stats=True))
    args.worker_index = 0
    args.coord_dir = tmp_path
    args.address = 'ipc://fixture'
    result = harness.measure(args)
    assert result['ok'] is False
    assert result['summary']['attempts'] == 3
    assert (result['summary']['successes'], result['summary']['failures'], result['summary']['timeouts']) == (1, 1, 1)
    assert len(result['failures']) == 2
    records = [json.loads(line) for line in Path(result['samples_path']).read_text().splitlines()]
    assert records[0]['provenance']['source_commit'] == 'fixture'
    assert len(records) == 7  # metadata + two warmups + three attempts + hold validation
    assert [record['status'] for record in records if record.get('measurement')] == ['success', 'timeout', 'error']


def test_numeric_public_deadline_error_is_a_timeout(harness, tmp_path):
    from ipc_evidence import SampleLog
    class CallDeadlineExceeded(Exception):
        code = 715  # IntEnum stringification on Python 3.11+ is numeric.
    def expire(_):
        raise CallDeadlineExceeded('call deadline exceeded')
    with SampleLog(tmp_path, {'role': 'normal', 'pid': 123, 'worker_index': 0}, {}, {}) as log:
        _, sample = harness.call_sample(log, 0, expire, b'x')
    assert sample['status'] == 'timeout'
    assert sample['error_type'] == 'CallDeadlineExceeded'


def test_worker_failure_before_barrier_keeps_original_report(harness, monkeypatch, tmp_path):
    failure = {'pid': 999, 'ok': False, 'failures': ['original native import failure'],
               'error_type': 'ImportError', 'traceback_tail': 'original traceback'}
    class Child:
        pid = 999
        returncode = 1
        def __init__(self, *args, **kwargs):
            pass
        def poll(self):
            return 1
        def communicate(self, timeout=None):
            return json.dumps(failure), 'native import stderr'
    monkeypatch.setattr(harness.subprocess, 'Popen', Child)
    monkeypatch.setattr(harness, 'source_commit', lambda: 'fixture')
    monkeypatch.setattr(harness.tempfile, 'mkdtemp', lambda **kwargs: str(tmp_path / 'ipc'))
    monkeypatch.setenv('C2_IPC_ROOT', 'fixture-original')
    args = harness.row_namespace(dict(harness.matrix_rows()[0], python=sys.executable,
        topology='pairs', output_dir=str(tmp_path)))
    result = harness.run_controller(args)
    assert result['ok'] is False
    assert result['workers'][0]['failures'] == ['original native import failure']
    assert json.loads((tmp_path / 'child-999.stdout.json').read_text()) == failure
    assert (tmp_path / 'child-999.stderr.log').read_text() == 'native import stderr'
    assert result['child_processes'][0]['exit_code'] == 1


def test_cleanup_budget_charge_cannot_pass(harness):
    stats = {'runtime_outgoing': None, 'server': None,
        'retired': [{'role': 'runtime_outgoing', 'cells': {'file': {'used_bytes': 4096}}}],
        'holds': {'active_holds': 0}}
    failures = []
    harness.require_no_charges({'available': True, 'stats': stats}, 'cleanup', failures)
    assert failures and 'runtime_outgoing/file=4096' in failures[0]
