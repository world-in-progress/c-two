"""Review boundaries: source ownership, native tool lookup and clean inputs."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

import pytest

from tools.dev import test_local as runner


def git_checkout(tmp_path):
    root = tmp_path / 'checkout'
    root.mkdir()
    git = shutil.which('git')
    def call(*args):
        return subprocess.run([git, '-C', str(root), *args], check=True,
                              capture_output=True, encoding='utf-8').stdout.strip()
    call('init')
    (root / 'tracked.txt').write_text('original', encoding='utf-8')
    call('add', '.')
    call('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'initial')
    return root, call('rev-parse', 'HEAD'), git


@pytest.mark.parametrize('mutation', ['tracked', 'head', 'untracked', 'none'])
def test_main_freezes_source_identity_before_real_steps(tmp_path, monkeypatch, mutation):
    root, initial, git = git_checkout(tmp_path)
    monkeypatch.setattr(runner, 'ROOT', root)
    monkeypatch.setattr(runner, 'check', lambda *_: {'status': 'passed', 'tools': {}, 'errors': []})
    real_probe = runner.probe
    monkeypatch.setattr(runner, 'probe', lambda command, env, cwd=None: real_probe(command, env, cwd or root))
    script = ''
    if mutation in ('tracked', 'head'):
        script = "from pathlib import Path; Path('tracked.txt').write_text('changed')"
    if mutation == 'untracked':
        script = "from pathlib import Path; Path('new-source.py').write_text('changed')"
    if mutation == 'head':
        script += '; import subprocess; subprocess.run(' + repr([git, 'add', 'tracked.txt']) + ', check=True)'
        script += '; subprocess.run(' + repr([git, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                                              'commit', '-m', 'changed']) + ', check=True)'
    monkeypatch.setattr(runner, 'suite_steps', lambda *_: [runner.Step('plain', [sys.executable, '-c', script], cwd=root)])
    output = root / 'evidence'
    code = runner.main(['run', '--suite', 'cli', '--output', str(output)])
    report = json.loads((output / 'run.json').read_text(encoding='utf-8'))
    assert report['source_sha'] == initial
    assert report['source_identity_before']['source_sha'] == initial
    if mutation == 'head':
        assert report['source_identity_after']['source_sha'] != initial
    else:
        assert report['source_identity_after']['source_sha'] == initial
    assert code == (0 if mutation == 'none' else 1)
    assert report['status'] == ('passed' if mutation == 'none' else 'failed')
    if mutation != 'none':
        assert 'source' in report['source_error'].lower()


@pytest.mark.parametrize('conflicting_parent', [False, True])
def test_execution_selects_child_path_tool_on_real_platform(tmp_path, monkeypatch, conflicting_parent):
    selected = tmp_path / 'selected'
    parent = tmp_path / 'parent'
    selected.mkdir()
    parent.mkdir()
    if os.name == 'nt':
        system = Path(os.environ['SystemRoot']) / 'System32'
        tool = selected / 'c2-review-tool.exe'
        shutil.copy2(system / 'cmd.exe', tool)
        if conflicting_parent:
            shutil.copy2(system / 'where.exe', parent / tool.name)
        args = ['/d', '/c', 'echo selected-tool']
    else:
        tool = selected / 'c2-review-tool'
        tool.write_text("#!/bin/sh\nprintf 'selected-tool\\n'\n", encoding='utf-8')
        tool.chmod(0o755)
        if conflicting_parent:
            other = parent / tool.name
            other.write_text("#!/bin/sh\nprintf 'wrong-tool\\n'\n", encoding='utf-8')
            other.chmod(0o755)
        args = []
    env = dict(os.environ)
    env['PATH'] = str(selected)
    monkeypatch.setenv('PATH', str(parent))
    assert runner.probe(['c2-review-tool', *args], env, tmp_path) == 'selected-tool'
    output = tmp_path / 'evidence'
    report = runner.execute_steps([runner.Step('plain', ['c2-review-tool', *args])], output, env, tmp_path, 20)
    assert report['status'] == 'passed'
    assert Path(report['steps'][0]['command'][0]).resolve() == tool.resolve()
    assert 'selected-tool' in (output / 'plain/output.log').read_text(encoding='utf-8')


@pytest.mark.parametrize('suite', ['core', 'all'])
def test_core_preparation_installs_compiler_at_actual_sibling_path(tmp_path, monkeypatch, suite):
    root = tmp_path / 'c-two'
    root.mkdir()
    sibling = root.parent / 'fastdb/ts/fastdb4ts'
    sibling.mkdir(parents=True)
    for name in ('package.json', 'package-lock.json'):
        (sibling / name).write_text('{}', encoding='utf-8')
    monkeypatch.setattr(runner, 'ROOT', root)
    monkeypatch.setattr(runner, 'probe', lambda *args: '' if 'status' in args[0] else runner.strict.FASTDB_SHA)
    node = tmp_path / 'node.exe'
    cli = tmp_path / 'node_modules/npm/bin/npm-cli.js'
    cli.parent.mkdir(parents=True)
    cli.touch()
    monkeypatch.setattr(runner, 'find_tool', lambda name, env: str(node))
    flags = ['prepare', '--suite', suite, '--output', str(tmp_path / 'logs'), '--coresdk-lib', str(tmp_path)]
    if suite == 'all':
        for name in ('fastdb', 'c2-mem', 'typescript'):
            package = tmp_path / f'{name}.tgz'
            package.touch()
            flags += [f'--{name}-package', str(package)]
    options = runner.parser().parse_args(flags)
    steps = runner.preparation_steps(options, tmp_path / 'inputs')
    expected = root.parent / 'fastdb/ts/fastdb4ts'
    stage = next(step for step in steps if step.name == 'core-typescript-dependencies')
    assert stage.command == runner.npm_command(node, 'ci', '--ignore-scripts', '--prefix', str(expected))
    assert {'node', 'npm'} <= set(runner.required_tools(suite, prepare=True))


@pytest.mark.parametrize('phase', ['planning', 'probe'])
def test_prepare_exceptions_retain_failed_json(tmp_path, monkeypatch, phase):
    monkeypatch.setattr(runner, 'check', lambda *_: {'status': 'passed', 'tools': {}, 'errors': []})
    if phase == 'probe':
        def fail(*_):
            raise subprocess.TimeoutExpired('probe', 30)
        monkeypatch.setattr(runner, 'check', fail)
    else:
        def fail(*_):
            raise runner.LocalError('controlled plan failure')
        monkeypatch.setattr(runner, 'preparation_steps', fail)
    output = tmp_path / 'evidence'
    assert runner.main(['prepare', '--suite', 'cli', '--output', str(output)]) == 1
    report = json.loads((output / 'run.json').read_text(encoding='utf-8'))
    assert report['status'] == 'failed'
    assert report['phase'] == ('preflight' if phase == 'probe' else 'prepare-plan')
    assert report['error']
    assert report['finished_at']


def test_existing_evidence_is_not_overwritten_on_error(tmp_path):
    output = tmp_path / 'evidence'
    output.mkdir()
    marker = output / 'run.json'
    marker.write_text('original', encoding='utf-8')
    assert runner.main(['prepare', '--suite', 'cli', '--output', str(output)]) == 1
    assert marker.read_text(encoding='utf-8') == 'original'


def test_aggregate_stays_running_until_final_source_validation(tmp_path, monkeypatch):
    root, _, _ = git_checkout(tmp_path)
    monkeypatch.setattr(runner, 'ROOT', root)
    monkeypatch.setattr(runner, 'check', lambda *_: {'status': 'passed'})
    monkeypatch.setattr(runner, 'suite_steps', lambda *_: [runner.Step('plain', [sys.executable, '-c', 'pass'], cwd=root)])
    output = root / 'evidence'
    actual = runner.source_identity
    calls = []
    def capture(*args):
        if calls:
            retained = json.loads((output / 'run.json').read_text(encoding='utf-8'))
            assert retained['status'] == 'running'
            assert 'finished_at' not in retained
        calls.append(True)
        return actual(*args)
    monkeypatch.setattr(runner, 'source_identity', capture)
    assert runner.main(['run', '--suite', 'cli', '--output', str(output)]) == 0
    assert len(calls) == 2


def test_core_check_fingerprints_compiler_implementation(tmp_path, monkeypatch):
    root = tmp_path / 'c-two'
    root.mkdir()
    compiler = root.parent / 'fastdb/ts/fastdb4ts/node_modules/typescript/bin/tsc'
    compiler.parent.mkdir(parents=True)
    compiler.write_text('require("../lib/tsc.js")', encoding='utf-8')
    implementation = compiler.parent.parent / 'lib/_tsc.js'
    implementation.parent.mkdir()
    implementation.write_text('original', encoding='utf-8')
    monkeypatch.setattr(runner, 'ROOT', root)
    monkeypatch.setattr(runner, 'required_tools', lambda *_args, **_kwargs: [])
    monkeypatch.setattr(runner, 'find_tool', lambda *_: sys.executable)
    monkeypatch.setattr(runner, 'probe', lambda *_: 'Version 5.9.3')
    options = runner.parser().parse_args(['check', '--suite', 'core', '--output', str(tmp_path / 'out')])
    report = runner.check(options, dict(os.environ))
    assert str(implementation) in report['input_hashes']
    runner.validate_input_hashes(report['input_hashes'])
    implementation.write_text('changed', encoding='utf-8')
    with pytest.raises(runner.LocalError, match='input changed'):
        runner.validate_input_hashes(report['input_hashes'])
