"""Local sidecar planning and real child-process evidence, without native imports."""

import importlib
import json
import os
from pathlib import Path
import signal
import sys

import pytest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))


def local():
    return importlib.import_module('tools.dev.test_local')


def test_complete_suite_order_and_source_link_modes():
    runner = local()
    assert runner.selected_suites('all') == ['core', 'cli', 'rust-sdk', 'native', 'node', 'repo', 'python']
    options = runner.parser().parse_args(['run', '--suite', 'all', '--output', 'new'])
    steps = runner.suite_steps(options, ROOT)
    assert [s.name for s in steps] == runner.selected_suites('all')
    assert {s.name for s in steps if s.link_mode == 'source'} == {'cli', 'native'}
    assert '--all-features' in next(s.command for s in steps if s.name == 'rust-sdk')
    assert '--confcutdir=tests/repo' in next(s.command for s in steps if s.name == 'repo')
    python = next(s for s in steps if s.name == 'python')
    assert 'tools/dev/test_python.py' in python.command
    assert '--workers' in python.command and python.command[python.command.index('--workers') + 1] == '1'


def test_selected_suite_dependencies_do_not_require_node_for_cli():
    runner = local()
    assert 'node' not in runner.required_tools('cli', prepare=False)
    assert {'cargo', 'git'} <= set(runner.required_tools('cli', prepare=False))
    assert {'node', 'npm', 'cmake', 'ninja'} <= set(runner.required_tools('typescript', prepare=True))


def test_child_environment_isolated_and_loader_path_scoped(tmp_path):
    runner = local()
    lib = tmp_path.resolve()
    original = {'PATH': 'original', 'C2_RELAY_ANCHOR_ADDRESS': 'http://unwanted', 'CTWO_TEST_RUN_NAMESPACE': 'old', 'CARGO_TARGET_DIR': 'foreign-build'}
    env = runner.child_environment(original, [], lib, 'win32')
    assert original['PATH'] == 'original'
    assert env['PATH'].split(os.pathsep)[0] == str(lib)
    assert env['C2_RELAY_ANCHOR_ADDRESS'] == '' and env['PYTHONUTF8'] == '1'
    assert 'CTWO_TEST_RUN_NAMESPACE' not in env
    assert 'CARGO_TARGET_DIR' not in env
    assert env['FASTDB_PAYLOAD_LINK_MODE'] == 'system'
    assert env['MSBUILDDISABLENODEREUSE'] == '1'


def test_failed_step_never_passes_and_stops_preparation(tmp_path):
    runner = local()
    marker = tmp_path / 'must-not-exist'
    steps = [runner.Step('failure', [sys.executable, '-c', 'raise SystemExit(7)']),
             runner.Step('later', [sys.executable, '-c', f'from pathlib import Path; Path({str(marker)!r}).touch()'])]
    report = runner.execute_steps(steps, tmp_path / 'out', dict(os.environ), tmp_path, 20, stop_on_failure=True)
    assert report['status'] == 'failed' and report['steps'][0]['exit_code'] == 7
    assert not marker.exists()
    assert report['steps'][1]['status'] == 'blocked'


def test_output_directory_cannot_be_reused(tmp_path):
    runner = local()
    output = tmp_path / 'exists'
    output.mkdir()
    with pytest.raises(runner.LocalError, match='already exists'):
        runner.execute_steps([], output, dict(os.environ), ROOT, 20)


def test_passing_child_produces_development_evidence(tmp_path):
    runner = local()
    output = tmp_path / 'out'
    report = runner.execute_steps([runner.Step('plain', [sys.executable, '-c', 'print("你好")'])],
                                  output, {**os.environ, 'PYTHONUTF8': '1'}, tmp_path, 20)
    assert report['status'] == 'passed'
    assert report['evidence_stage'] == 'development'
    assert report['steps'][0]['cleanup_confirmed']
    assert json.loads((output / 'run.json').read_text(encoding='utf-8'))['status'] == 'passed'
    assert '你好' in (output / 'plain/output.log').read_text(encoding='utf-8')


def test_npm_uses_actual_selected_node_cli(tmp_path):
    runner = local()
    node = tmp_path / ('node.exe' if os.name == 'nt' else 'node')
    cli = tmp_path / 'node_modules/npm/bin/npm-cli.js'
    cli.parent.mkdir(parents=True)
    cli.touch()
    node.touch()
    assert runner.npm_command(node, 'ci') == [str(node), str(cli), 'ci']


def test_prepare_rejects_partial_package_inputs():
    runner = local()
    options = runner.parser().parse_args(['prepare', '--suite', 'typescript', '--output', 'new', '--fastdb-package', 'one.tgz'])
    with pytest.raises(runner.LocalError, match='together'):
        runner.validate_options(options)


def test_timeout_must_be_finite():
    runner = local()
    options = runner.parser().parse_args(['run', '--suite', 'repo', '--output', 'new', '--stage-timeout', 'nan'])
    with pytest.raises(runner.LocalError, match='finite'):
        runner.validate_options(options)


def test_prepare_orders_paired_wasm_before_pack(tmp_path, monkeypatch):
    runner = local()
    monkeypatch.setattr(runner, 'probe', lambda command, env: '' if 'status' in command else runner.strict.FASTDB_SHA)
    node = tmp_path / 'node.exe'
    cli = tmp_path / 'node_modules/npm/bin/npm-cli.js'
    cli.parent.mkdir(parents=True)
    cli.touch()
    monkeypatch.setattr(runner, 'find_tool', lambda name, env: str(node))
    options = runner.parser().parse_args(['prepare', '--suite', 'typescript', '--output', str(tmp_path / 'logs'), '--emsdk', str(tmp_path / 'emsdk')])
    steps = runner.preparation_steps(options, tmp_path / 'inputs')
    names = [s.name for s in steps]
    assert names.index('fastdb-wasm-build') < names.index('fastdb-assets') < names.index('fastdb-pack')
    assert names.index('c2-mem-build') < names.index('c2-mem-pack')
    assert '--ignore-scripts' in next(s.command for s in steps if s.name == 'fastdb-pack')
    assert any('checkout' in s.command and runner.strict.FASTDB_SHA in s.command for s in steps)


def test_unknown_junit_skip_cannot_pass(tmp_path):
    runner = local()
    output = tmp_path / 'out'
    xml = output / 'repo/junit.xml'
    script = f'from pathlib import Path; Path({str(xml)!r}).write_text(\'<testsuites><testsuite tests="1" skipped="1"><testcase classname="unknown" name="test_case"><skipped message="missing dependency"/></testcase></testsuite></testsuites>\')'
    report = runner.execute_steps([runner.Step('repo', [sys.executable, '-c', script])], output, dict(os.environ), tmp_path, 20)
    assert report['status'] == 'failed'
    assert 'skip' in report['steps'][0]['error']


def test_tap_unknown_skip_and_todo_rejected():
    runner = local()
    with pytest.raises(runner.LocalError, match='skip'):
        runner.test_counts('node', 'ok 1 - unknown # SKIP missing input\n# tests 1\n# pass 0\n# fail 0\n# skipped 1\n# todo 0\n', None)


def test_rust_counts_preserve_ignored_and_reject_unknown_ignore():
    runner = local()
    with pytest.raises(runner.LocalError, match='ignored'):
        runner.test_counts('core', 'test some_unknown ... ignored\ntest result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.1s\n', None)


def test_repo_preparation_provides_official_release_fixtures_without_native_build(tmp_path, monkeypatch):
    monkeypatch.delenv('C2_DEV_FASTDB_RELEASE_FIXTURES', raising=False)
    runner = local()
    options = runner.parser().parse_args(['prepare', '--suite', 'repo', '--output', str(tmp_path / 'logs')])
    steps = runner.preparation_steps(options, tmp_path / 'inputs')
    assert [s.name for s in steps] == ['repo-venv', 'repo-dependencies', 'release-fixtures']
    assert all('sync' not in s.command for s in steps)
    assert 'cargo' not in runner.required_tools('repo', prepare=True)


def test_supplied_prebuilt_packages_must_exist_for_prepare(tmp_path):
    runner = local()
    options = runner.parser().parse_args(['prepare', '--suite', 'typescript', '--output', str(tmp_path / 'logs'),
                                          '--fastdb-package', 'absent1', '--c2-mem-package', 'absent2', '--typescript-package', 'absent3'])
    with pytest.raises(runner.LocalError, match='missing'):
        runner.validate_options(options)


def test_main_sigterm_is_cancelled_and_handler_restored(tmp_path, monkeypatch):
    runner = local()
    previous = signal.getsignal(signal.SIGTERM)
    def interrupted(options, environment):
        handler = signal.getsignal(signal.SIGTERM)
        assert callable(handler) and handler != previous
        handler(signal.SIGTERM, None)
    monkeypatch.setattr(runner, 'check', interrupted)
    assert runner.main(['check', '--suite', 'cli', '--output', str(tmp_path / 'output')]) == 130
    assert signal.getsignal(signal.SIGTERM) == previous
    assert json.loads((tmp_path / 'output/run.json').read_text(encoding='utf-8'))['status'] == 'cancelled'


def test_native_prepare_has_dependency_stage(tmp_path):
    runner = local()
    options = runner.parser().parse_args(['prepare', '--suite', 'native', '--output', str(tmp_path / 'logs')])
    steps = runner.preparation_steps(options, tmp_path / 'inputs')
    assert [s.name for s in steps] == ['native-dependencies', 'python-sync']
    assert steps[0].link_mode == 'source'
    assert steps[0].command == ['cargo', 'fetch', '--locked', '--manifest-path', 'sdk/python/native/Cargo.toml']
