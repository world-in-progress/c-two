"""Actual sibling provenance boundaries, with real Git and controlled tools."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

import pytest

from tools.dev import test_local as runner


def checkout(path):
    path.mkdir()
    git = shutil.which('git')
    def call(*args):
        return subprocess.run([git, '-C', str(path), *args], check=True,
                              capture_output=True, encoding='utf-8').stdout.strip()
    call('init')
    payload = path / 'ts/fastdb4ts/src/payload/index.ts'
    payload.parent.mkdir(parents=True)
    payload.write_text('export {}', encoding='utf-8')
    (path / '.gitignore').write_text('node_modules/\n', encoding='utf-8')
    for name in ('package.json', 'package-lock.json'):
        (payload.parents[2] / name).write_text('{}', encoding='utf-8')
    call('add', '.')
    call('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'initial')
    return call, call('rev-parse', 'HEAD'), payload


def fixture(tmp_path, monkeypatch):
    root = tmp_path / 'c-two'
    checkout(root)
    sibling = tmp_path / 'fastdb'
    call, head, payload = checkout(sibling)
    compiler = sibling / 'ts/fastdb4ts/node_modules/typescript/bin/tsc'
    compiler.parent.mkdir(parents=True)
    compiler.write_text('compiler fixture', encoding='utf-8')
    monkeypatch.setattr(runner, 'ROOT', root)
    monkeypatch.setattr(runner.strict, 'FASTDB_SHA', head)
    monkeypatch.setattr(runner, 'required_tools', lambda *_args, **_kwargs: [])
    monkeypatch.setattr(runner, 'find_tool', lambda *_: sys.executable)
    real_probe = runner.probe
    def probe(command, env, cwd=None):
        if command[0] == 'git':
            return real_probe([shutil.which('git'), *command[1:]], env, cwd or root)
        if runner.strict.PROBE in command:
            raise runner.LocalError('Python probe is outside this Core source fixture')
        return 'Version 5.9.3'
    monkeypatch.setattr(runner, 'probe', probe)
    monkeypatch.setattr(runner, 'npm_command', lambda *_args: [sys.executable, '-c', 'pass'])
    return root, sibling, call, head, payload


@pytest.mark.parametrize('action', ['check', 'prepare', 'run'])
@pytest.mark.parametrize('violation', ['wrong-head', 'dirty', 'untracked'])
@pytest.mark.parametrize('suite', ['core', 'all'])
def test_core_rejects_unverified_actual_sibling(tmp_path, monkeypatch, action, violation, suite):
    root, sibling, call, head, payload = fixture(tmp_path, monkeypatch)
    if violation == 'wrong-head':
        call('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '--allow-empty', '-m', 'wrong revision')
    elif violation == 'dirty':
        payload.write_text('changed', encoding='utf-8')
    else:
        (sibling / 'new-source.ts').write_text('changed', encoding='utf-8')
    options = runner.parser().parse_args([action, '--suite', suite, '--output', str(tmp_path / 'out'),
                                         '--fastdb-source', str(root)])
    report = runner.check(options, dict(os.environ))
    assert report['status'] == 'failed'
    assert any('FastDB sibling' in error for error in report['errors'])
    if action == 'prepare':
        with pytest.raises(runner.LocalError, match='FastDB sibling'):
            runner.preparation_steps(options, tmp_path / 'inputs')


@pytest.mark.parametrize('mutation', ['none', 'dirty', 'head'])
def test_main_records_and_rechecks_sibling_source(tmp_path, monkeypatch, mutation):
    root, sibling, call, head, payload = fixture(tmp_path, monkeypatch)
    lib = tmp_path / 'lib'
    lib.mkdir()
    from tools.dev.test_platform import core_library_name
    (lib / core_library_name(sys.platform)).touch()
    (lib / 'fastdb.lib').touch()
    script = '' if mutation == 'none' else f'from pathlib import Path; Path({str(payload)!r}).write_text("changed")'
    if mutation == 'head':
        script += '; import subprocess; subprocess.run(' + repr([shutil.which('git'), '-C', str(sibling), 'add', '.']) + ', check=True)'
        script += '; subprocess.run(' + repr([shutil.which('git'), '-C', str(sibling), '-c', 'user.name=Fixture',
                                              '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'changed']) + ', check=True)'
    script += ';' if script else ''
    script += 'print("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;")'
    monkeypatch.setattr(runner, 'suite_steps', lambda *_: [runner.Step('core', [sys.executable, '-c', script], cwd=root)])
    output = tmp_path / 'evidence'
    code = runner.main(['run', '--suite', 'core', '--output', str(output), '--coresdk-lib', str(lib)])
    report = json.loads((output / 'run.json').read_text(encoding='utf-8'))
    assert report['preflight']['core_fastdb']['source_sha'] == head
    assert report['core_fastdb_after']['source_sha'] == (call('rev-parse', 'HEAD'))
    assert code == (0 if mutation == 'none' else 1)
    assert report['status'] == ('passed' if mutation == 'none' else 'failed')
    if mutation != 'none':
        assert 'FastDB sibling' in report['core_fastdb_error']


def test_linked_worktree_is_a_valid_actual_sibling(tmp_path, monkeypatch):
    root, sibling, call, head, payload = fixture(tmp_path, monkeypatch)
    worktree = tmp_path / 'linked/fastdb'
    call('worktree', 'add', '--detach', str(worktree), head)
    linked_root = worktree.parent / 'c-two'
    linked_root.mkdir()
    monkeypatch.setattr(runner, 'ROOT', linked_root)
    options = runner.parser().parse_args(['prepare', '--suite', 'core', '--output', str(tmp_path / 'out')])
    report = runner.check(options, dict(os.environ))
    assert report['status'] == 'passed'
    assert report['core_fastdb']['source_sha'] == head
    assert Path(report['core_fastdb']['path']) == worktree.resolve()


def test_change_after_preflight_blocks_core_child(tmp_path, monkeypatch):
    root, sibling, call, head, payload = fixture(tmp_path, monkeypatch)
    lib = tmp_path / 'lib'
    lib.mkdir()
    from tools.dev.test_platform import core_library_name
    (lib / core_library_name(sys.platform)).touch()
    (lib / 'fastdb.lib').touch()
    marker = tmp_path / 'must-not-execute'
    def plan(*_):
        payload.write_text('changed after preflight', encoding='utf-8')
        return [runner.Step('core', [sys.executable, '-c', f'from pathlib import Path; Path({str(marker)!r}).touch()'], cwd=root)]
    monkeypatch.setattr(runner, 'suite_steps', plan)
    output = tmp_path / 'evidence'
    assert runner.main(['run', '--suite', 'core', '--output', str(output), '--coresdk-lib', str(lib)]) == 1
    report = json.loads((output / 'run.json').read_text(encoding='utf-8'))
    assert not marker.exists()
    assert 'FastDB sibling' in report['steps'][0]['error']
    assert report['preflight']['core_fastdb'] != report['core_fastdb_after']


def test_prepare_mutation_retains_failed_source_report(tmp_path, monkeypatch):
    root, sibling, call, head, payload = fixture(tmp_path, monkeypatch)
    monkeypatch.setattr(runner, 'preparation_steps', lambda *_: [runner.Step(
        'prepare-probe', [sys.executable, '-c', f'from pathlib import Path; Path({str(payload)!r}).write_text("changed")'], cwd=root)])
    output = tmp_path / 'evidence'
    assert runner.main(['prepare', '--suite', 'core', '--output', str(output)]) == 1
    report = json.loads((output / 'run.json').read_text(encoding='utf-8'))
    assert report['steps'][0]['status'] == 'passed'
    assert report['status'] == 'failed'
    assert report['preflight']['core_fastdb']['source_sha'] == head
    assert 'FastDB sibling' in report['core_fastdb_error']


@pytest.mark.parametrize('flag', ['--assume-unchanged', '--skip-worktree'])
def test_hidden_tracked_payload_edits_cannot_claim_pinned_source(tmp_path, monkeypatch, flag):
    root, sibling, call, head, payload = fixture(tmp_path, monkeypatch)
    call('update-index', flag, 'ts/fastdb4ts/src/payload/index.ts')
    payload.write_text('hidden local change', encoding='utf-8')
    assert call('status', '--porcelain') == ''
    options = runner.parser().parse_args(['prepare', '--suite', 'core', '--output', str(tmp_path / 'out')])
    report = runner.check(options, dict(os.environ))
    assert report['status'] == 'failed'
    assert any('FastDB sibling' in error for error in report['errors'])
