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
def test_matrix_requires_successful_process_and_complete_identity(
    harness, monkeypatch, tmp_path, case, returncode, expected_exit,
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
            self.returncode = returncode

        def communicate(self, timeout=None):
            return json.dumps(record), ''

        def poll(self):
            return self.returncode

        def kill(self):
            pytest.fail('a completed row must not be signalled')

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
    if returncode != 0:
        assert result['totals']['passed'] == 0
        assert result['totals']['failed'] == 1
    if case == 'server_hash_mismatch':
        assert result['native']['agreement'] is False
