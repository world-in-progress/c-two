"""Independent stream benchmark evidence; these tests do not start a runtime."""
from __future__ import annotations

import importlib.util
from pathlib import Path
import pickle
import sys
from types import SimpleNamespace

import pytest


ROOT = Path(__file__).resolve().parents[2]


@pytest.fixture
def stream():
    spec = importlib.util.spec_from_file_location(
        '_ipc_stream_test', ROOT / 'tools/benchmarks/ipc_stream.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_rows_are_candidate_only_and_keep_budget_expectations(stream):
    rows = stream.matrix_rows()
    assert len(rows) == len({row['name'] for row in rows}) == 23
    zero = next(row for row in rows if row['name'] == 'all_backing_zero')
    assert [stream.expected_capacity(zero, mode) for mode in stream.MODES] == [True, True, False]
    for row in rows:
        if row.get('budget_offset') is not None:
            assert [stream.expected_capacity(row, mode) for mode in stream.MODES] == [
                False, False, row['budget_offset'] < 0]


@pytest.mark.parametrize('size', [4095, 4096, 4097, 65535, 65536, 65537, 262144])
def test_boundaries_use_encoded_size_including_pickle_header(stream, size):
    value = stream.sized_bytes(size, 'boundary')
    assert len(pickle.dumps(value, protocol=4)) == size
    assert len(value) < size


def test_quantiles_are_nearest_rank_and_empty_is_unavailable(stream):
    assert stream.quantiles([]) == {'p50_ns': None, 'p95_ns': None, 'p99_ns': None}
    assert stream.quantiles(list(range(1, 101))) == {
        'p50_ns': 50, 'p95_ns': 95, 'p99_ns': 99}


def test_receipt_checks_count_terminal_and_raw_sample_math(stream):
    row = {'count': 2, 'name': 'fixture'}
    run = {'mode': 'S', 'start_ns': 10, 'end_ns': 50, 'first_ns': 20,
           'last_ns': 40, 'items': 2, 'logical_bytes': 4,
           'terminal': {'completed': True, 'error': None, 'complete_items': 2},
           'samples': [{'start_ns': 10, 'end_ns': 20, 'latency_ns': 10, 'status': 'success'},
                       {'start_ns': 25, 'end_ns': 40, 'latency_ns': 15, 'status': 'success'}],
           'capacity_error': False, 'integrity_ok': True}
    assert stream.summarize_run(row, run)['terminal_latency_ns'] == 40
    for key, value in [('items', 1), ('integrity_ok', False)]:
        changed = dict(run, **{key: value})
        with pytest.raises(stream.BenchmarkError):
            stream.summarize_run(row, changed)
    changed = dict(run, terminal={'completed': True, 'error': None, 'complete_items': 1})
    with pytest.raises(stream.BenchmarkError):
        stream.summarize_run(row, changed)
    changed = dict(run, samples=[dict(run['samples'][0], latency_ns=99), run['samples'][1]])
    with pytest.raises(stream.BenchmarkError):
        stream.summarize_run(row, changed)


def test_overlap_requires_a_complete_short_call_inside_a_large_call(stream):
    short = [dict(start_ns=10, end_ns=30), dict(start_ns=35, end_ns=40),
             dict(start_ns=25, end_ns=55)]
    calls = [dict(start_ns=10, end_ns=30), dict(start_ns=50, end_ns=60)]
    assert stream.overlapping(short, [dict(mode='Q', samples=calls)]) == short[:1]
    assert stream.overlapping(short, [dict(mode='S', start_ns=10, end_ns=60)]) == short


def test_workflow_runs_candidate_source_interpreter_and_retains_separate_evidence(monkeypatch, tmp_path):
    import yaml
    workflow = ROOT / '.github/workflows/ipc-memory-paired.yml'
    config = yaml.load(workflow.read_text(), Loader=yaml.BaseLoader)
    assert set(config['on']) == {'workflow_dispatch'}
    steps = config['jobs']['paired']['steps']
    step = next(step for step in steps if step.get('id') == 'candidate_stream')
    assert "steps.build_environments.outcome == 'success'" in step['if']
    assert '!cancelled()' in step['if']
    calls = []
    monkeypatch.setenv('GITHUB_WORKSPACE', str(tmp_path))
    monkeypatch.setenv('RUNNER_TEMP', str(tmp_path / 'runner-temp'))
    monkeypatch.setitem(sys.modules, 'source_validation', SimpleNamespace(
        source_mode_environment=lambda env: dict(env, source_mode_test='yes')))
    monkeypatch.setattr('subprocess.run', lambda command, **kwargs: calls.append((command, kwargs)))
    old_path = list(sys.path)
    try:
        exec(compile(step['run'], str(workflow), 'exec'), {})
    finally:
        sys.path[:] = old_path
    assert len(calls) == 1
    command, options = calls[0]
    candidate = tmp_path / 'candidate/c-two'
    assert options['cwd'] == candidate and options['check']
    assert options['env']['source_mode_test'] == 'yes'
    assert Path(command[0]).is_relative_to(candidate / '.venv')
    assert command[command.index('--output-dir') + 1] == str(tmp_path / 'runner-temp/ipc-stream')
    upload = next(step for step in steps if step.get('name') == 'Retain candidate stream comparison evidence')
    assert upload['if'] == 'always()' and upload['with']['retention-days'] == '30'
    assert upload['with']['path'] == '${{ runner.temp }}/ipc-stream/'


def test_standalone_mixed_diagnostic_is_recomputed_and_rejects_forged_native(stream, tmp_path, monkeypatch):
    import hashlib
    import json
    monkeypatch.setattr(stream, 'ROOT', tmp_path)
    native_file = tmp_path / 'fixture-native'
    native_file.write_bytes(b'fixture')
    identity = dict(source_commit='fixture', sha256=hashlib.sha256(b'fixture').hexdigest(),
                    module_path=str(native_file), harness_sha256=stream.bench.native_digest(stream.__file__))
    row = dict(name='mixed', kind='mixed', workers=1, count=32)
    samples = [dict(kind='sample', measurement=True, request_id=str(i), start_ns=2*i,
                    end_ns=2*i+1, latency_ns=1, integrity_ok=True, status='success') for i in range(500)]
    header = dict(kind='run', config=row, provenance=identity)
    (tmp_path / 'short.jsonl').write_text(''.join(json.dumps(s) + '\n' for s in [header, *samples]))
    (tmp_path / 'large.jsonl').write_text(json.dumps(header) + '\n')
    run = dict(mode='alone', job_id='0/alone', short_samples=samples, large_runs=[],
               selected_short_ids=[s['request_id'] for s in samples], short_latency=stream.quantiles([1]*500),
               large_samples_path='large.jsonl')
    worker = dict(index=0, provenance=identity, samples_path='short.jsonl', runs=[run])
    report = dict(row=row, provenance=identity, workers=[worker], phase='diagnostic')
    summary = stream.verify_phase(report, tmp_path, row, 1, ('alone',), 'fixture')
    assert set(summary) == {'alone'}
    assert summary['alone']['overlapping_samples'] == 500
    native_file.write_bytes(b'changed')
    with pytest.raises(stream.BenchmarkError, match='native hash differs'):
        stream.verify_phase(report, tmp_path, row, 1, ('alone',), 'fixture')


def test_transport_uses_actual_backing_and_stream_budget_peaks(stream):
    row = dict(no_shm=True)
    server = {'server': {'cells': {'shm': {'peak_bytes': 0}}}}
    client = {'runtime_outgoing': {'cells': {'file': {'peak_bytes': 0}, 'reassembly': {'peak_bytes': 0}}},
              'payload_buffer': {'peak_bytes': 100_000}}
    stream.verify_transport(row, 'S', server, client, 100_000, 200_000)
    with pytest.raises(stream.BenchmarkError, match='file reassembly scenario'):
        stream.verify_transport(row, 'U', server, client, 100_000, 200_000)
    client['runtime_outgoing']['cells']['file']['peak_bytes'] = 200_000
    client['runtime_outgoing']['cells']['reassembly']['peak_bytes'] = 200_000
    stream.verify_transport(row, 'U', server, client, 100_000, 200_000)
    with pytest.raises(stream.BenchmarkError, match='unexpectedly used unary reassembly'):
        stream.verify_transport(row, 'S', server, client, 100_000, 200_000)
