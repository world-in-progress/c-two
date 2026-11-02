"""Anomaly benchmark contracts without starting IPC, SHM, or child processes."""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import sys
from types import ModuleType, SimpleNamespace

import pytest


ROOT = Path(__file__).resolve().parents[2]
BENCHMARKS = ROOT / 'tools' / 'benchmarks'


@pytest.fixture
def anomaly(monkeypatch):
    monkeypatch.syspath_prepend(str(BENCHMARKS))
    spec = importlib.util.spec_from_file_location('_ipc_anomaly_contract_test', BENCHMARKS / 'ipc_anomaly.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def options(tmp_path, scenario='stopped', control='parallel'):
    return SimpleNamespace(
        scenario=scenario, control=control, calls=2, warmup=0, abnormal_calls=3,
        pause_seconds=0, call_timeout=1, child_timeout=1, output_dir=tmp_path,
        python=sys.executable,
    )


@pytest.mark.parametrize('scenario,expected', [
    ('hold', (1, 32, 32, 32)),
    ('fallback_dedicated', (2, 32, 32, 32)),
    ('fallback_inline', (2, 1, 32, 32)),
    ('fallback_file', (2, 1, 0, 32)),
    ('exhausted', (2, 1, 0, 0)),
])
def test_scenario_budget_geometry(anomaly, tmp_path, scenario, expected):
    config = anomaly.scenario_spec(options(tmp_path, scenario))
    inflight, server_shm, normal_shm, normal_file = expected
    assert config['inflight_calls'] == inflight
    assert config['response_bytes'] == 300 * 1024
    assert config['server_overrides']['pool_segment_size'] == anomaly.MIB
    assert config['server_overrides']['max_pool_segments'] == 1
    assert config['server_overrides']['shm_backing_budget_bytes'] == server_shm * anomaly.MIB
    assert config['normal_overrides']['shm_backing_budget_bytes'] == normal_shm * anomaly.MIB
    assert config['normal_overrides']['file_backing_budget_bytes'] == normal_file * anomaly.MIB
    assert config['abnormal_overrides']['shm_backing_budget_bytes'] == 32 * anomaly.MIB
    if scenario == 'exhausted':
        assert config['server_overrides']['live_reassembly_budget_bytes'] == 0
        assert config['normal_overrides']['live_reassembly_budget_bytes'] == 0


def memory_snapshot(used=0, fd=8):
    scope = {'role': 'server', 'cells': {'shm': {'used_bytes': used, 'peak_bytes': used}}}
    return {
        'memory': {'stats': {'runtime_outgoing': None, 'server': scope, 'retired': []}},
        'os': {
            'fd_count': {'value': fd}, 'handle_count': {'value': None},
            'mapping_count': {'value': None},
        },
    }


def test_convergence_fails_on_backing_or_os_leak(anomaly):
    baseline = memory_snapshot()
    backing = anomaly.convergence(baseline, memory_snapshot(used=4096))
    assert backing['budget_converged'] is False
    assert backing['converged'] is False
    handles = anomaly.convergence(baseline, memory_snapshot(fd=9))
    assert handles['budget_converged'] is True
    assert handles['os']['fd_count']['converged'] is False
    assert handles['converged'] is False
    assert anomaly.convergence(baseline, memory_snapshot())['converged'] is True


def test_linux_convergence_checks_shared_mappings_and_retains_total_vmas(anomaly):
    before, after = memory_snapshot(), memory_snapshot(fd=6)
    before['os']['mapping_count'] = {'value': 251, 'shared_value': 1}
    after['os']['mapping_count'] = {'value': 339, 'shared_value': 1}
    receipt = anomaly.convergence(before, after)
    assert receipt['converged'] is True
    assert receipt['os']['mapping_count']['before_total'] == 251
    assert receipt['os']['mapping_count']['after_total'] == 339
    after['os']['mapping_count']['shared_value'] = 2
    assert anomaly.convergence(before, after)['converged'] is False


def test_linux_mapping_counts_include_deleted_shm_and_file_backings(anomaly):
    import ipc_evidence
    counts = ipc_evidence.linux_mapping_counts('''\
1000-2000 rw-p 00000000 00:00 0
2000-3000 ---p 00000000 00:00 0
3000-4000 rw-s 00000000 00:1a 1 /dev/shm/c2-backing (deleted)
4000-5000 rw-s 00000000 00:1b 2 /tmp/c2-spill (deleted)
5000-6000 r--p 00000000 00:1c 3 /lib/python.so
''')
    assert counts['value'] == 5
    assert counts['shared_value'] == 2


def install_fake_cc(monkeypatch, anomaly):
    cc = ModuleType('c_two')
    cc._native = SimpleNamespace()
    cc.ConcurrencyConfig = lambda **kwargs: SimpleNamespace(**kwargs)
    cc.set_transport_policy = lambda **kwargs: None
    cc.set_server = lambda **kwargs: None
    cc.set_client = lambda **kwargs: None
    cc.register = lambda *args, **kwargs: None
    cc.server_address = lambda: 'ipc://fixture'
    cc.connect = lambda *args, **kwargs: object()
    cc.with_call_options = lambda client, **kwargs: client
    cc.close = lambda client: None
    cc.shutdown = lambda **kwargs: {'completed': True}
    monkeypatch.setitem(sys.modules, 'c_two', cc)
    monkeypatch.setattr(anomaly, 'workload_types', lambda *_args: (object(), object))
    monkeypatch.setattr(anomaly.bench, 'runtime_provenance', lambda native: {
        'module_path': '/fixture/native.so', 'sha256': 'a' * 64,
    })
    monkeypatch.setattr(anomaly.bench, 'source_commit', lambda: 'fixture-commit')
    monkeypatch.setattr(anomaly, '_snapshot', lambda log, phase: memory_snapshot())
    monkeypatch.setattr(anomaly, 'process_snapshot', lambda pid=None: {})
    return cc


def raw_worker(anomaly, output, role, pid, index, count, config):
    provenance = {'module_path': '/fixture/native.so', 'sha256': 'a' * 64,
                  'source_commit': 'fixture-commit'}
    identity = {'role': role, 'pid': pid, 'worker_index': index}
    with anomaly.SampleLog(output, identity, config, provenance) as log:
        for request_id in range(count):
            log.sample(request_id, request_id * 10, request_id * 10 + 5,
                       'success', integrity_ok=True)
        log.event('closed')
    return {**identity, 'ok': True, 'failures': [], 'config': config,
            'summary': anomaly.summarize_samples(log.samples),
            'native_path': provenance['module_path'], 'native_sha256': provenance['sha256'],
            'provenance': provenance, 'samples_path': str(log.samples_path),
            'events_path': str(log.events_path)}


def test_controller_gates_stop_release_normal_resume_and_reap(anomaly, monkeypatch, tmp_path):
    cc = install_fake_cc(monkeypatch, anomaly)
    registrations = []
    cc.register = lambda *args, **kwargs: registrations.append(kwargs['concurrency'])
    trace = []
    original_publish = anomaly.publish

    def publish(path, value=None):
        trace.append('publish:' + path.name)
        original_publish(path, value)

    def wait_marker(path, *_args, **_kwargs):
        trace.append('wait:' + path.name)
        return {}

    monkeypatch.setattr(anomaly, 'publish', publish)
    monkeypatch.setattr(anomaly, 'wait_marker', wait_marker)
    monkeypatch.setattr(anomaly, 'wait_for_entered', lambda *args: trace.append('entered_calls_ready') or 1)
    monkeypatch.setattr(anomaly.bench, 'release_workers', lambda *args: None)
    monkeypatch.setattr(anomaly, 'suspend_process', lambda proc: trace.append('suspend'))
    monkeypatch.setattr(anomaly, 'resume_process', lambda proc: trace.append('resume'))
    if os.name != 'nt':
        monkeypatch.setattr(anomaly.os, 'waitpid', lambda pid, flags: (pid, 0))
        monkeypatch.setattr(anomaly.os, 'WIFSTOPPED', lambda status: True)

    class Child:
        next_pid = 2000

        def __init__(self, args, **kwargs):
            self.pid = Child.next_pid
            Child.next_pid += 1
            self.role = json.loads(args[-1])['role']
            self.spec = json.loads(args[-1])
            self.returncode = None

        def poll(self):
            return self.returncode

        def communicate(self, timeout=None):
            self.returncode = 0
            trace.append('communicate:' + self.role)
            return json.dumps(raw_worker(anomaly, tmp_path, self.role, self.pid,
                self.spec['index'], 6 if self.role == 'normal' else 1,
                self.spec['config'])), ''

        def wait(self, timeout=None):
            return self.returncode

    monkeypatch.setattr(anomaly.subprocess, 'Popen', Child)
    monkeypatch.setattr(anomaly.bench, 'kill_owned_child', lambda child: pytest.fail('unexpected kill'))
    report = anomaly.run_anomaly(options(tmp_path))
    assert report['ok'] is True, report['failures']
    assert len(registrations) == 1
    assert vars(registrations[0]) == {'mode': 'parallel', 'max_workers': None, 'max_pending': None}
    assert report['internal_probes'] == anomaly.INTERNAL_PROBES
    assert all(not item['available'] and item['reason'] for item in report['internal_probes'].values())
    order = [
        'entered_calls_ready', 'suspend', 'publish:method-release.json',
        'wait:returning-abnormal-0.json', 'publish:normal-during.json',
        'wait:normal-during-done.json', 'resume', 'communicate:abnormal',
        'publish:normal-after.json', 'communicate:normal',
    ]
    positions = [trace.index(item) for item in order]
    assert positions == sorted(positions)
    events = [json.loads(line) for line in Path(report['benchmark']['events_path']).read_text().splitlines()]
    names = [event['event'] for event in events]
    assert names.index('resource_gated') < names.index('reader_stopped') < names.index('abnormal_ready')
    assert names.index('normal_released') < names.index('reader_resumed') < names.index('os_wait')
    assert [event['pid'] for event in events if event['event'] == 'os_wait'] == [2001, 2000]


def test_controller_rejects_missing_normal_attempts(anomaly, monkeypatch, tmp_path):
    install_fake_cc(monkeypatch, anomaly)
    monkeypatch.setattr(anomaly.bench, 'release_workers', lambda *args: None)
    monkeypatch.setattr(anomaly, 'wait_marker', lambda *args, **kwargs: {})

    class Child:
        pid = 3000
        returncode = None

        def __init__(self, *args, **kwargs):
            pass

        def poll(self):
            return self.returncode

        def communicate(self, timeout=None):
            self.returncode = 0
            return json.dumps(raw_worker(anomaly, tmp_path, 'normal', self.pid, 0, 5,
                anomaly.scenario_spec(options(tmp_path, scenario='hold', control='alone')))), ''

        def wait(self, timeout=None):
            return self.returncode

    monkeypatch.setattr(anomaly.subprocess, 'Popen', Child)
    report = anomaly.run_anomaly(options(tmp_path, scenario='hold', control='alone'))
    assert report['ok'] is False
    assert any('[standalone:tool_failure] normal client has missing raw attempts' == failure
               for failure in report['failures'])
    assert json.loads((tmp_path / 'anomaly.json').read_text())['ok'] is False


@pytest.mark.parametrize('control,during_status,during_error,expected_ok', [
    ('parallel', 'error', 'memory budget cell exhausted', True),
    ('parallel', 'success', None, False),
    ('parallel', 'error', 'unrelated failure', False),
    ('alone', 'success', None, True),
    ('alone', 'error', 'memory budget cell exhausted', False),
])
def test_expected_capacity_failure_is_call_stage_and_parallel_only(
    anomaly, monkeypatch, tmp_path, control, during_status, during_error, expected_ok,
):
    install_fake_cc(monkeypatch, anomaly)
    util = ModuleType('c_two.transport.client.util')
    util.ping = lambda address, timeout: True
    monkeypatch.setitem(sys.modules, 'c_two.transport', ModuleType('c_two.transport'))
    monkeypatch.setitem(sys.modules, 'c_two.transport.client', ModuleType('c_two.transport.client'))
    monkeypatch.setitem(sys.modules, 'c_two.transport.client.util', util)
    monkeypatch.setattr(anomaly.bench, 'wait_for_release', lambda *args: None)
    monkeypatch.setattr(anomaly, 'wait_marker', lambda *args: {})

    def invoke(log, proxy, request_id, request_bytes, response_bytes, phase, **kwargs):
        status = during_status if phase == 'during' else 'success'
        error = during_error if phase == 'during' else None
        sample = log.sample(request_id, 1, 2, status, error_type='RuntimeError' if error else None,
                            error_message=error, phase=phase)
        return sample, []

    monkeypatch.setattr(anomaly, '_invoke', invoke)
    config = anomaly.scenario_spec(options(tmp_path, 'exhausted', control))
    config['calls'] = 1
    result = anomaly.anomaly_worker({
        'role': 'normal', 'index': 0, 'address': 'ipc://fixture',
        'coord_dir': str(tmp_path), 'output_dir': str(tmp_path), 'config': config,
    })
    assert result['ok'] is expected_ok, result['failures']
    assert result['summary']['attempts'] == 3
    assert result['by_phase']['during']['failures'] == (during_status == 'error')
    assert result['cleanup']['shutdown']['completed'] is True
    if not expected_ok:
        assert result['failures']
        category = 'scenario_not_established' if during_status == 'success' else 'correctness_failure'
        assert all(f'[standalone:{category}]' in failure for failure in result['failures'])


SCENARIOS = (
    'hold', 'slow', 'stopped', 'fallback_dedicated', 'fallback_inline',
    'fallback_file', 'exhausted', 'release_idle', 'release_busy',
    'release_blocked', 'release_batch',
)


@pytest.mark.parametrize('scenario', SCENARIOS)
def test_overrides_match_their_native_role_key_sets(anomaly, tmp_path, scenario):
    import re
    source = (ROOT / 'core/foundation/c2-config/src/ipc.rs').read_text()
    def native_keys(name):
        body = re.search(rf'pub const {name}:.*?= &\[(.*?)\];', source, re.S).group(1)
        return set(re.findall(r'"([a-z_]+)"', body))
    server_keys = native_keys('SERVER_IPC_OVERRIDE_KEYS')
    client_keys = native_keys('CLIENT_IPC_OVERRIDE_KEYS')
    config = anomaly.scenario_spec(options(tmp_path, scenario))
    assert set(config['server_overrides']) <= server_keys
    for role in ('normal', 'abnormal'):
        assert set(config[role + '_overrides']) <= client_keys
    expected_frame_size = 16 * anomaly.MIB if scenario == 'release_blocked' else 64 * 1024
    assert config['server_overrides']['max_frame_size'] == expected_frame_size


@pytest.mark.parametrize('abnormal_calls', [1, 65, 80, 128])
def test_release_batch_has_execution_space_for_every_gated_call_and_normal_client(anomaly, tmp_path, abnormal_calls):
    args = options(tmp_path, 'release_batch')
    args.abnormal_calls = abnormal_calls
    config = anomaly.scenario_spec(args)
    assert config['server_overrides']['max_execution_workers'] >= config['inflight_calls'] + 1
    assert config['route_concurrency'] == {'mode': 'parallel', 'max_workers': None, 'max_pending': None}


@pytest.mark.parametrize('child_exited', [False, True])
def test_incomplete_resource_entry_reports_observed_and_expected_counts(anomaly, tmp_path, child_exited):
    for index in (0, 1, 3):
        anomaly.publish(tmp_path / f'entered-abnormal-{index}.json')
    children = [SimpleNamespace(poll=lambda: 1 if child_exited else None)]
    expected_error = RuntimeError if child_exited else TimeoutError
    with pytest.raises(expected_error, match=r'entered=3, expected=80'):
        anomaly.wait_for_entered(tmp_path, 80, 0, children)


def test_resource_entry_barrier_ignores_other_markers_and_releases_only_when_complete(anomaly, tmp_path):
    for filename in ('entered-normal-0.json', 'returning-abnormal-0.json', 'entered-abnormal-80.json'):
        anomaly.publish(tmp_path / filename)
    for index in range(80):
        anomaly.publish(tmp_path / f'entered-abnormal-{index}.json')
    assert anomaly.wait_for_entered(tmp_path, 80, 0, [SimpleNamespace(poll=lambda: None)]) == 80


def test_native_config_check_covers_every_scenario_and_role_without_starting_host(anomaly, monkeypatch):
    check_spec = importlib.util.spec_from_file_location('_check_anomaly_config', BENCHMARKS / 'check_anomaly_config.py')
    check = importlib.util.module_from_spec(check_spec)
    check_spec.loader.exec_module(check)
    cc = install_fake_cc(monkeypatch, anomaly)
    calls = []
    def resolve(role, overrides, global_overrides):
        calls.append((role, overrides, global_overrides))
        if role == 'client' and 'max_frame_size' in overrides:
            raise ValueError('server-only option passed to client')
        return overrides
    cc._native.resolve_server_ipc_config = lambda **kwargs: resolve('server', **kwargs)
    cc._native.resolve_client_ipc_config = lambda **kwargs: resolve('client', **kwargs)
    cc.register = lambda *args, **kwargs: pytest.fail('config validation started a Host')
    cc.connect = lambda *args, **kwargs: pytest.fail('config validation connected a client')
    report = check.check_configs(80)
    assert report['ok'] is True, report['failures']
    assert report['native_resolutions_attempted'] == report['native_resolutions_passed'] == 66
    assert len(calls) == 66
    assert sum(role == 'server' for role, *_ in calls) == 22
    assert sum(role == 'client' for role, *_ in calls) == 44
    assert {(row['scenario'], row['control']) for row in report['rows']} == {
        (scenario, control) for scenario in SCENARIOS for control in ('alone', 'parallel')
    }
    batch = [row for row in report['rows'] if row['scenario'] == 'release_batch']
    assert all(row['resolved']['server']['max_execution_workers'] == 81 for row in batch)
