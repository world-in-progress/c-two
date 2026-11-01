"""Simulated paired-runner evidence, without starting IPC or SHM."""
from __future__ import annotations

import importlib.util
import json
from pathlib import Path
from types import SimpleNamespace

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[2]
RUNNER = ROOT / 'tools/benchmarks/ipc_paired.py'
EVIDENCE_SPEC = importlib.util.spec_from_file_location(
    '_ipc_evidence_paired_test', ROOT / 'tools/benchmarks/ipc_evidence.py')
EVIDENCE = importlib.util.module_from_spec(EVIDENCE_SPEC)
EVIDENCE_SPEC.loader.exec_module(EVIDENCE)


@pytest.fixture
def paired():
    spec = importlib.util.spec_from_file_location('_ipc_paired_test', RUNNER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def summary(p50=100, p99=200, throughput=2_000_000_000 / 300):
    return {'attempts': 2, 'successes': 2, 'failures': 0, 'timeouts': 0,
            'unsuccessful': 0, 'failure_rate': 0.0,
            'p50_ns': p50, 'p95_ns': 200, 'p99_ns': p99,
            'elapsed_ns': 300,
            'throughput_calls_per_s': throughput,
            'quantile_method': 'nearest-rank: sorted[ceil(p*n)-1]',
            'quantile_population': 'all measurement attempts, including errors and timeouts'}


def worker(tmp_path, values=None):
    identity = {'role': 'normal', 'pid': 1, 'worker_index': 0}
    provenance = {'module_path': '/native.so', 'sha256': 'a' * 64, 'source_commit': 'fixture'}
    with EVIDENCE.SampleLog(tmp_path, identity, {}, provenance) as log:
        log.sample('warmup-0', 0, 10, 'success', phase='warmup',
                   integrity_ok=True, measurement=False)
        log.sample(1, 0, 100, 'success', integrity_ok=True)
        log.sample(2, 100, 300, 'success', integrity_ok=True)
        log.event('start')
    log.samples_path.rename(tmp_path / 'samples.jsonl')
    log.events_path.rename(tmp_path / 'events.jsonl')
    return {**identity, 'summary': values or summary(), 'samples_path': 'samples.jsonl',
            'events_path': 'events.jsonl', 'native_path': '/native.so',
            'native_sha256': 'a' * 64, 'provenance': provenance}


def set_run_config(tmp_path, config):
    path = tmp_path / 'samples.jsonl'
    records = [json.loads(line) for line in path.read_text().splitlines()]
    records[0]['config'] = config
    path.write_text(''.join(json.dumps(record) + '\n' for record in records))


def test_normal_worker_requires_raw_samples_and_success(paired, tmp_path):
    record = worker(tmp_path)
    assert paired.check_worker(record, tmp_path, True) == record['summary']
    lines = (tmp_path / 'samples.jsonl').read_text().splitlines()
    (tmp_path / 'samples.jsonl').write_text('\n'.join(lines[:-1]) + '\n')
    with pytest.raises(paired.EvidenceError, match='sample count'):
        paired.check_worker(record, tmp_path, True)
    worker(tmp_path)
    record['summary']['failures'] = 1
    with pytest.raises(paired.EvidenceError, match='differs from raw samples'):
        paired.check_worker(record, tmp_path, True)


def test_matrix_rejects_missing_row_and_missing_sample(paired, tmp_path):
    spec = {'workers': 1, 'calls': 2, 'pool_enabled': True, 'segment_size': 1024,
            'payload_bytes': 1, 'shm_threshold': 4096, 'ipc_overrides': {},
            'expect_error_substring': None, 'warmup': 1, 'call_timeout': 30.0}
    evidence_worker = worker(tmp_path)
    set_run_config(tmp_path, {key: value for key, value in spec.items() if key != 'workers'})
    record = {'ok': True, 'row': {'name': paired.NORMAL_ROWS[0]},
              'spec': spec, 'benchmark': {'ok': True,
              'workers': [evidence_worker]}}
    (tmp_path / 'rows').mkdir()
    (tmp_path / 'rows' / 'row.json').write_text(json.dumps(record))
    matrix = {'rows': [{'name': paired.NORMAL_ROWS[0], 'ok': True, 'exit_code': 0,
                       'result_file': 'rows/row.json', 'native_sha256': 'a' * 64}],
              'native': {'single_installed_hash': 'a' * 64}, 'failures': []}
    (tmp_path / 'matrix.json').write_text(json.dumps(matrix))
    with pytest.raises(paired.EvidenceError, match='rows mismatch'):
        paired.check_matrix(tmp_path, set(paired.NORMAL_ROWS[:2]))
    assert paired.check_matrix(tmp_path, {paired.NORMAL_ROWS[0]})[paired.NORMAL_ROWS[0]] == [summary()]
    (tmp_path / 'samples.jsonl').unlink()
    with pytest.raises(paired.EvidenceError, match='missing samples'):
        paired.check_matrix(tmp_path, {paired.NORMAL_ROWS[0]})


def test_anomaly_checks_normal_raw_evidence_and_native_identity(paired, tmp_path):
    normal = dict(worker(tmp_path), role='normal')
    path = tmp_path / 'samples.jsonl'
    records = [json.loads(line) for line in path.read_text().splitlines()]
    for index in range(4):
        start = 300 + index * 100
        records.append({'kind': 'sample', 'identity': records[0]['identity'],
                        'request_id': index + 3, 'start_ns': start, 'end_ns': start + 100,
                        'latency_ns': 100, 'status': 'success', 'integrity_ok': True,
                        'measurement': True})
    path.write_text(''.join(json.dumps(record) + '\n' for record in records))
    normal['summary'].update(attempts=6, successes=6, p50_ns=100, p95_ns=200,
                             p99_ns=200, elapsed_ns=700,
                             throughput_calls_per_s=6_000_000_000 / 700)
    set_run_config(tmp_path, {'calls': 2})
    report = {'ok': True, 'failures': [], 'scenario': 'hold', 'control': 'alone',
              'source_commit': 'fixture', 'config': {'calls': 2},
              'benchmark': {'workers': [normal], 'server': {'native_sha256': 'a' * 64}}}
    (tmp_path / 'anomaly.json').write_text(json.dumps(report))
    result = paired.check_anomaly(tmp_path, 'hold', 'alone', 'fixture')
    assert result['normal_summaries'] == [normal['summary']]
    report['benchmark']['server']['native_sha256'] = 'b' * 64
    (tmp_path / 'anomaly.json').write_text(json.dumps(report))
    with pytest.raises(paired.EvidenceError, match='hashes disagree'):
        paired.check_anomaly(tmp_path, 'hold', 'alone', 'fixture')


def test_paired_ratios_are_per_worker_and_report_only(paired):
    name = paired.NORMAL_ROWS[0]
    groups = []
    for _ in range(5):
        groups.append({'baseline': {name: [summary(100, 200, 1000), summary(100, 200, 1000)]},
                       'candidate': {name: [summary(106, 221, 940),
                                            summary(90, 180, 1100)]}})
    with pytest.raises(paired.EvidenceError, match='at least five'):
        paired.ratio_report(groups[:4], {name})
    report = paired.ratio_report(groups, {name})[name]
    assert report[0]['median_ratio'] == {'p50': 1.06, 'p99': 1.105, 'throughput': 0.94}
    assert report[0]['thresholds_report_only'] == {
        'p50_at_most_1_05': False, 'p99_at_most_1_10': False,
        'throughput_at_least_0_95': False}
    assert all(report[1]['thresholds_report_only'].values())


def test_order_alternates_ab_ba(paired):
    assert [paired.paired_order(i) for i in range(5)] == [
        ('baseline', 'candidate'), ('candidate', 'baseline'),
        ('baseline', 'candidate'), ('candidate', 'baseline'),
        ('baseline', 'candidate')]


def args(tmp_path, paired):
    baseline = tmp_path / 'baseline'
    candidate = tmp_path / 'candidate'
    baseline.mkdir()
    candidate.mkdir()
    baseline_python = baseline / 'python'
    candidate_python = candidate / 'python'
    baseline_python.touch()
    candidate_python.touch()
    return SimpleNamespace(baseline_root=baseline, candidate_root=candidate,
                           baseline_python=baseline_python, candidate_python=candidate_python,
                           output_dir=tmp_path / 'output', baseline_commit='b861f994',
                           baseline_ref=None, candidate_ref=None,
                           pairs=5, only=None, skip_anomalies=True)


def test_dependency_mismatch_fails_before_any_run(paired, monkeypatch, tmp_path):
    options = args(tmp_path, paired)
    monkeypatch.setattr(paired, 'git_head', lambda root: 'b861f994' if root == options.baseline_root else 'candidate')
    monkeypatch.setattr(paired, 'python_identity',
                        lambda python, root: {'fastdb4py': '0.2.3' if root == options.baseline_root else '0.2.4'})
    monkeypatch.setattr(paired, 'run_harness', lambda *args, **kwargs: pytest.fail('ran despite mismatch'))
    with pytest.raises(paired.EvidenceError, match='dependency mismatch'):
        paired.run(options)


def test_five_complete_groups_and_alternating_invocations(paired, monkeypatch, tmp_path):
    options = args(tmp_path, paired)
    options.only = [paired.NORMAL_ROWS[0]]
    monkeypatch.setattr(paired, 'git_head', lambda root: 'b861f994' if root == options.baseline_root else 'candidate')
    monkeypatch.setattr(paired, 'python_identity', lambda *args: {'dependencies': {'fastdb4py': '0.2.4'}})
    calls = []
    def fake_harness(_options, label, group, kind):
        calls.append((group, label, kind))
        return tmp_path
    monkeypatch.setattr(paired, 'run_harness', fake_harness)
    monkeypatch.setattr(paired, 'check_matrix',
                        lambda directory, selected, commit: {paired.NORMAL_ROWS[0]: [summary()]})
    result = paired.run(options)
    assert result['ok'] is True
    assert len(result['groups']) == 5
    assert calls == [(i, label, 'matrix') for i in range(5)
                     for label in paired.paired_order(i)]
    assert (options.output_dir / 'receipt.json').is_file()


def test_row_config_mismatch_fails_pair(paired, monkeypatch, tmp_path):
    options = args(tmp_path, paired)
    options.only = [paired.NORMAL_ROWS[0]]
    monkeypatch.setattr(paired, 'git_head', lambda root: 'b861f994' if root == options.baseline_root else 'candidate')
    monkeypatch.setattr(paired, 'python_identity', lambda *args: {'dependencies': {}})
    monkeypatch.setattr(paired, 'run_harness', lambda *_args: tmp_path)
    call_index = 0
    def different_specs(_directory, _selected, _commit):
        nonlocal call_index
        call_index += 1
        return {paired.NORMAL_ROWS[0]: [summary()],
                '__specs__': {paired.NORMAL_ROWS[0]: {'payload_bytes': call_index}}}
    monkeypatch.setattr(paired, 'check_matrix', different_specs)
    result = paired.run(options)
    assert result['ok'] is False
    assert 'config mismatch' in result['failures'][0]
    assert len(result['groups']) == 5


def test_failed_invocation_keeps_later_groups(paired, monkeypatch, tmp_path):
    options = args(tmp_path, paired)
    options.only = [paired.NORMAL_ROWS[0]]
    monkeypatch.setattr(paired, 'git_head', lambda root: 'b861f994' if root == options.baseline_root else 'candidate')
    monkeypatch.setattr(paired, 'python_identity', lambda *args: {'dependencies': {}})
    calls = []
    def fake_harness(_options, label, group, kind):
        calls.append((group, label))
        if group == 0 and label == 'baseline':
            raise paired.EvidenceError('controller crash; process.log retained')
        return tmp_path
    monkeypatch.setattr(paired, 'run_harness', fake_harness)
    monkeypatch.setattr(paired, 'check_matrix', lambda *_args: {
        paired.NORMAL_ROWS[0]: [summary()], '__specs__': {paired.NORMAL_ROWS[0]: {}}})
    result = paired.run(options)
    assert result['ok'] is False
    assert len(calls) == 10
    assert len(result['groups']) == 5
    assert 'controller crash' in result['failures'][0]


def test_workflow_keeps_manual_separate_source_environments():
    workflow = yaml.load((ROOT / '.github/workflows/ipc-memory-paired.yml').read_text(),
                         Loader=yaml.BaseLoader)
    assert set(workflow['on']) == {'workflow_dispatch'}
    assert set(workflow['on']['workflow_dispatch']['inputs']) == {'baseline_ref', 'candidate_ref'}
    assert workflow['env']['FASTDB_PAYLOAD_LINK_MODE'] == 'system'
    assert set(workflow['jobs']['paired']['strategy']['matrix']['os']) == {
        'ubuntu-24.04', 'macos-15', 'windows-2025', 'windows-11-arm'}
    steps = workflow['jobs']['paired']['steps']
    checkouts = [step['with']['path'] for step in steps if 'with' in step and
                 step['with'].get('path') in {'baseline/c-two', 'candidate/c-two'}]
    assert checkouts == ['baseline/c-two', 'candidate/c-two']
    assert any('prepare_fastdb_sdk.py' in step.get('run', '') for step in steps)
    assert any(step.get('if') == 'always()' and step.get('uses') == 'actions/upload-artifact@v4'
               for step in steps)
