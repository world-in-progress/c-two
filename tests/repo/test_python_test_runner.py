"""仅用临时目录/pure pytest 验证入口；不导入native、不运行SHM。"""

import ast
import json
import os
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace
from typing import Any

import pytest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from tools.dev import test_python as runner

pytestmark = pytest.mark.skipif(os.name != 'posix', reason='local Python runner uses POSIX process groups and flock; Windows has native gates')


@pytest.mark.parametrize('groups', [
    {'ordinary': ['a'], 'matrix': ['c']},  # 缺b
    {'ordinary': ['a', 'b'], 'matrix': ['b', 'c']},  # 重复b
    {'ordinary': ['a', 'b'], 'matrix': ['c', 'unexpected']},
    {'ordinary': [], 'matrix': ['a', 'b', 'c']},
])
def test_partition_rejects_loss_overlap_extra_and_empty(groups: dict[str, list[str]]) -> None:
    with pytest.raises(runner.RunnerError):
        runner.validate_partition(['a', 'b', 'c'], groups)


def test_partition_uses_nodeids_including_parameter_values() -> None:
    runner.validate_partition(['a[x]', 'a[y]', 'b'], {'ordinary': ['b'], 'matrix': ['a[y]', 'a[x]']})
    with pytest.raises(runner.RunnerError):
        runner.validate_partition(['a[x]', 'a[y]'], {'one': ['a[x]'], 'two': ['a[x]']})


def synthetic_suite(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, *, fail: bool=False, skip: bool=False) -> tuple[Path, SimpleNamespace, dict[str, str], dict]:
    root = tmp_path / 'checkout with spaces'
    root.mkdir()
    (root / 'pytest.ini').write_text('[pytest]\n')
    monkeypatch.setattr(runner, 'ROOT', root)
    files = {
        'sdk/python/tests/unit/test_alpha.py': 'import pytest\n@pytest.mark.parametrize("value", [1, 2])\ndef test_case(value):\n    assert value > 0\n',
        'sdk/python/tests/unit/test_beta.py': 'def test_case():\n    ' + ('assert False' if fail else 'assert True') + '\n',
        runner.PORTABLE[0]: 'def test_proof():\n    assert True\n',
        runner.PORTABLE[1]: 'import os\nROWS=[]\ndef test_row():\n    assert "PYTEST_XDIST_WORKER" not in os.environ\n    ROWS.append(1)\ndef test_receipt():\n    assert ROWS == [1]\n',
        runner.TYPESCRIPT: 'import os\nROWS=[]\ndef test_row():\n    assert "PYTEST_XDIST_WORKER" not in os.environ\n    ROWS.append(1)\ndef test_receipt():\n    assert ROWS == [1]\n',
    }
    if skip:
        files['sdk/python/tests/unit/test_beta.py'] = 'import pytest\ndef test_case():\n    pytest.skip("missing input")\n'
    for relative, text in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    (root / 'sdk/python/tests/conftest.py').write_text(
        'from pathlib import Path\nimport pytest\n'
        + fixture_source({'pytest_configure', 'pytest_collection_modifyitems', '_SERIAL_MATRIX_FILES'})
    )
    c3 = root / 'prebuilt c3'
    native = root / 'native bytes'
    c3.write_bytes(b'c3')
    native.write_bytes(b'native')
    environment = {k: v for k, v in os.environ.items() if not k.startswith(('PYTEST_', 'CTWO_'))}
    environment.update(PYTHONPATH=str(ROOT), PYTHONDONTWRITEBYTECODE='1')
    options = SimpleNamespace(python=sys.executable, workers=2, stage_timeout=20, c3=c3)
    info = {'c3_sha256': runner.sha256(c3), 'native': str(native), 'native_sha256': runner.sha256(native)}
    monkeypatch.setattr(runner, 'validate_receipts', lambda *args: None)
    return root, options, environment, info


@pytest.mark.parametrize('fail,skip,expected', [(False, False, 0), (True, False, 1), (False, True, 1)])
def test_real_pure_pytest_execution_and_matrix_serial(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fail: bool, skip: bool, expected: int) -> None:
    root, options, environment, info = synthetic_suite(tmp_path, monkeypatch, fail=fail, skip=skip)
    evidence = {'steps': []}
    output = root / 'new evidence'
    assert runner.execute(options, output, environment, info, evidence) == expected
    plan = json.loads((output / 'plan.json').read_text())
    assert len(plan['full']) == 8  # 参数化两项 + 普通一项 + proof一项 + 两个累积矩阵各两项
    ordinary = next(s for s in evidence['steps'] if s['id'] == 'ordinary')
    assert ordinary['exit_code'] == (1 if fail else 0)
    assert ordinary['status'] == ('passed' if expected == 0 else 'failed')
    assert ordinary['cleanup_confirmed']
    assert json.loads((output / 'ordinary/step.json').read_text())['status'] == ordinary['status']
    assert '--dist=loadfile' in ordinary['command']
    assert len(list((output / 'ordinary').glob('collected-gw*.json'))) == 2
    for name in ('portable', 'typescript'):
        step = next(s for s in evidence['steps'] if s['id'] == name)
        assert step['status'] == 'passed'  # 前一组普通失败仍运行完整矩阵
        assert step['command'][step['command'].index('-n') + 1] == '0'
        assert (output / name / 'junit.xml').is_file()
        assert (output / name / 'collected-master.json').is_file()
    calls = [r['nodeid'] for r in json.loads((output / 'ordinary/reports.json').read_text())['reports'] if r['when'] == 'call']
    assert set(calls) == set(plan['ordinary'])


def test_collection_skip_stops_before_execution(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    root, options, environment, info = synthetic_suite(tmp_path, monkeypatch)
    (root / 'sdk/python/tests/unit/test_beta.py').write_text('import pytest\npytest.skip("no native", allow_module_level=True)\n')
    evidence = {'steps': []}
    with pytest.raises(runner.RunnerError, match='skip'):
        runner.execute(options, root / 'evidence', environment, info, evidence)
    assert [s['id'] for s in evidence['steps']] == ['collect-full']


@pytest.mark.parametrize('platform,nodeid,phase,reason,accepted', [
    *((platform, nodeid, 'setup', 'Skipped: Windows named-pipe platform contract', True)
      for platform in ('darwin', 'linux') for nodeid in sorted(runner.WINDOWS_ENDPOINT_TESTS)),
    ('win32', runner.WINDOWS_ENDPOINT_TEST, 'setup', 'Skipped: Windows named-pipe platform contract', False),
    ('darwin', runner.WINDOWS_ENDPOINT_TEST, 'call', 'Skipped: Windows named-pipe platform contract', False),
    ('darwin', runner.WINDOWS_ENDPOINT_TEST, 'setup', 'Skipped: missing native', False),
    ('darwin', 'unexpected::test_skip', 'setup', 'Skipped: Windows named-pipe platform contract', False),
    ('darwin', 'tests/unit/test_endpoint_context.py::test_admin_probes_surface_windows_root_not_applicable[other-ping]',
     'setup', 'Skipped: Windows named-pipe platform contract', False),
])
def test_only_declared_windows_setup_skip_is_not_applicable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    platform: str, nodeid: str, phase: str, reason: str, accepted: bool,
) -> None:
    monkeypatch.setattr(runner.sys, 'platform', platform)
    (tmp_path / 'collected-master.json').write_text(json.dumps({
        'nodeids': [nodeid], 'collection_problems': [],
    }))
    (tmp_path / 'reports.json').write_text(json.dumps({
        'exit_status': 0,
        'reports': [
            {'nodeid': nodeid, 'when': phase, 'outcome': 'skipped', 'skip_reason': reason},
            {'nodeid': nodeid, 'when': 'teardown', 'outcome': 'passed'},
        ],
    }))
    (tmp_path / 'junit.xml').write_text('<testsuite/>')
    if accepted:
        runner.validate_execution(tmp_path, [nodeid], 1)
    else:
        with pytest.raises(runner.RunnerError):
            runner.validate_execution(tmp_path, [nodeid], 1)


def test_actual_collected_omission_stops_before_execution(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    root, options, environment, info = synthetic_suite(tmp_path, monkeypatch)
    original = runner.group_arguments
    def arguments(name: str) -> list[str]:
        return original(name) + (['--ignore=sdk/python/tests/unit/test_beta.py'] if name == 'ordinary' else [])
    monkeypatch.setattr(runner, 'group_arguments', arguments)
    evidence = {'steps': []}
    with pytest.raises(runner.RunnerError, match='test_beta'):
        runner.execute(options, root / 'evidence', environment, info, evidence)
    assert len(evidence['steps']) == 4 and not (root / 'evidence/ordinary').exists()


def test_raw_exit_log_and_timeout_are_retained(tmp_path: Path) -> None:
    failed = runner.run_step([sys.executable, '-c', 'print("failed evidence"); raise SystemExit(7)'], tmp_path / 'failed', os.environ.copy(), 5)
    assert failed['exit_code'] == 7
    assert failed['status'] == 'failed'
    assert 'failed evidence' in Path(failed['log']).read_text()
    timed = runner.run_step([sys.executable, '-c', 'import time; time.sleep(20)'], tmp_path / 'timeout', os.environ.copy(), 0.1)
    assert timed['status'] == 'timed_out'
    assert timed['process_exited'] and not timed['cleanup_confirmed']
    assert json.loads((tmp_path / 'timeout/step.json').read_text()) == timed


def test_timeout_stops_later_groups(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    root, options, environment, info = synthetic_suite(tmp_path, monkeypatch)
    real_step = runner.run_step
    def step(command: list[str], directory: Path, env: dict[str, str], timeout: float) -> dict:
        if directory.name == 'ordinary' and directory.parent.name == 'evidence':
            return real_step([sys.executable, '-c', 'import time; time.sleep(20)'], directory, env, .1)
        return real_step(command, directory, env, timeout)
    monkeypatch.setattr(runner, 'run_step', step)
    evidence = {'steps': []}
    with pytest.raises(runner.RunnerError, match='停止后续'):
        runner.execute(options, root / 'evidence', environment, info, evidence)
    assert evidence['steps'][-1]['id'] == 'ordinary'
    assert evidence['steps'][-1]['status'] == 'timed_out'
    assert not (root / 'evidence/portable').exists()


def test_selected_interpreter_and_path_arguments_are_not_shell_parsed(tmp_path: Path) -> None:
    python = str(tmp_path / 'chosen venv/bin/python')
    command = runner.build_pytest_command(python, 'ordinary', tmp_path / 'out with spaces', 4)
    assert command[0] == python
    assert '-n' in command and command[command.index('-n') + 1] == '4'
    assert '--max-worker-restart=0' in command
    assert f'--junitxml={tmp_path / "out with spaces/junit.xml"}' in command
    with pytest.raises(SystemExit):
        runner.parser().parse_args(['--c3', 'c3', '--output', 'out', '--workers', 'auto'])
    with pytest.raises(SystemExit):
        runner.parser().parse_args(['--c3', 'c3', '--output', 'out', '-k', 'omit'])


def test_resource_lease_rejects_another_user_and_releases(tmp_path: Path) -> None:
    with runner.resource_lease(tmp_path):
        with pytest.raises(runner.RunnerError, match='另一验证'):
            with runner.resource_lease(tmp_path):
                pytest.fail('租约未生效')
    with runner.resource_lease(tmp_path):
        pass


def test_resource_lease_contends_across_different_temp_directories(tmp_path: Path) -> None:
    other_tmp = tmp_path / 'other-tmp'
    other_tmp.mkdir()
    script = (
        'from pathlib import Path\n'
        'from tools.dev.test_python import resource_lease, RunnerError\n'
        f'try:\n    with resource_lease(Path({str(tmp_path)!r})):\n        raise SystemExit(7)\n'
        'except RunnerError:\n    print("lease rejected")\n'
    )
    env = {**os.environ, 'TMPDIR': str(other_tmp), 'TEMP': str(other_tmp), 'TMP': str(other_tmp), 'PYTHONPATH': str(ROOT)}
    with runner.resource_lease(tmp_path):
        child = subprocess.run([sys.executable, '-c', script], env=env, capture_output=True, text=True, timeout=5)
    assert child.returncode == 0, child.stdout + child.stderr
    assert 'lease rejected' in child.stdout


def fixture_source(names: set[str]) -> str:
    """执行公共fixture的真实函数，隔离其无关native导入。"""
    tree = ast.parse((ROOT / 'sdk/python/tests/conftest.py').read_text())
    selected = []
    for node in tree.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name in names:
            node.decorator_list = [d for d in node.decorator_list if not (isinstance(d, ast.Attribute) and d.attr == 'fixture')]
            node.returns = None
            for arg in (*node.args.posonlyargs, *node.args.args, *node.args.kwonlyargs):
                arg.annotation = None
            selected.append(node)
        if isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id in names for t in node.targets):
            selected.append(node)
    return ast.unparse(ast.Module(body=selected, type_ignores=[]))


def test_fixture_addresses_differ_across_workers_runs_and_processes(tmp_path: Path) -> None:
    source = fixture_source({'_address_counter', '_address_lock', '_run_namespace', '_next_id', 'unique_ipc_address'})
    script = 'import os,hashlib,uuid,threading\n' + source + '\nprint(unique_ipc_address())\nprint(unique_ipc_address())\n'
    seen = set()
    for run, worker in [('one', 'gw0'), ('one', 'gw1'), ('two', 'gw0'), ('one', 'gw0')]:
        env = {**os.environ, 'CTWO_TEST_RUN_NAMESPACE': run, 'PYTEST_XDIST_WORKER': worker}
        result = subprocess.run([sys.executable, '-c', script], env=env, capture_output=True, text=True, check=True)
        addresses = result.stdout.splitlines()
        assert len(addresses) == 2 and not (set(addresses) & seen)
        seen.update(addresses)


def test_fixture_rejects_direct_parallel_matrix_collection() -> None:
    namespace = {'Path': Path, 'pytest': pytest}
    exec(fixture_source({'pytest_collection_modifyitems', '_SERIAL_MATRIX_FILES'}), namespace)
    check = namespace['pytest_collection_modifyitems']
    config = SimpleNamespace(option=SimpleNamespace(numprocesses=2))
    check(config, [SimpleNamespace(path='unit/test_wire.py')])
    with pytest.raises(pytest.UsageError, match='单进程'):
        check(config, [SimpleNamespace(path=runner.PORTABLE[1])])
    serial = SimpleNamespace(option=SimpleNamespace(numprocesses=0))
    check(serial, [SimpleNamespace(path=runner.TYPESCRIPT)])
    serial.workerinput = {}
    with pytest.raises(pytest.UsageError):
        check(serial, [SimpleNamespace(path=runner.TYPESCRIPT)])


def test_direct_parallel_matrix_guard_stops_real_pytest_before_calls(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    root, options, environment, info = synthetic_suite(tmp_path, monkeypatch)
    source = 'from pathlib import Path\nimport pytest\n' + fixture_source({'pytest_configure', 'pytest_collection_modifyitems', '_SERIAL_MATRIX_FILES'})
    (root / 'sdk/python/tests/conftest.py').write_text(source)
    marker = root / 'partial receipt must not exist'
    (root / runner.TYPESCRIPT).write_text(f'from pathlib import Path\ndef test_row():\n    Path({str(marker)!r}).write_text("partial")\n')
    output = root / 'guard'
    command = runner.build_pytest_command(options.python, 'typescript', output, 2)
    command[command.index('-n') + 1] = '2'
    command[command.index('--dist=no')] = '--dist=loadfile'
    record = runner.run_step(command, output, runner.stage_environment(environment, output), 20)
    assert record['status'] == 'failed' and record['exit_code'] != 0
    assert not marker.exists()
    assert '单进程' in Path(record['log']).read_text()


def test_fixture_relay_lock_serializes_processes(tmp_path: Path) -> None:
    source = fixture_source({'_relay_start_thread_lock', '_relay_start_lock'})
    script = ('import os, tempfile, hashlib, threading, time\nfrom pathlib import Path\nfrom contextlib import contextmanager\n_run_namespace="test"\n'
              + source + '\nwith _relay_start_lock():\n    with open(os.environ["TRACE"], "a") as f:\n        f.write("start\\n"); f.flush(); time.sleep(.15); f.write("end\\n")\n')
    env = {**os.environ, 'CTWO_RELAY_START_LOCK': str(tmp_path / 'lock'), 'TRACE': str(tmp_path / 'trace')}
    procs = [subprocess.Popen([sys.executable, '-c', script], env=env) for _ in range(2)]
    assert [p.wait(timeout=5) for p in procs] == [0, 0]
    assert (tmp_path / 'trace').read_text().splitlines() == ['start', 'end', 'start', 'end']


def test_fixture_relay_logs_survive_close_with_test_identity(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import tempfile
    import uuid
    monkeypatch.setenv('CTWO_PYTHON_STAGE_DIR', str(tmp_path))
    monkeypatch.setenv('PYTEST_CURRENT_TEST', 'case[param] (setup)')
    namespace = {'Path': Path, 'os': os, 'tempfile': tempfile, 'uuid': uuid}
    exec(fixture_source({'_open_process_log'}), namespace)
    log = namespace['_open_process_log']('stderr')
    path = Path(log.name)
    log.write('relay failed detail\n')
    log.close()
    assert 'case[param]' in path.read_text() and 'relay failed detail' in path.read_text()


def test_main_preflight_failure_preserves_failure_receipt(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    def fail(*args: Any) -> None:
        raise runner.RunnerError('missing prebuilt input')
    monkeypatch.setattr(runner, 'preflight', fail)
    for key in ('PYTEST_ADDOPTS', 'PYTEST_PLUGINS', 'PYTEST_DISABLE_PLUGIN_AUTOLOAD'):
        monkeypatch.delenv(key, raising=False)
    output = tmp_path / 'evidence'
    assert runner.main(['--c3', str(tmp_path / 'missing c3'), '--output', str(output)]) == 1
    evidence = json.loads((output / 'run.json').read_text())
    assert evidence['status'] == 'failed' and evidence['exit_code'] == 1
    assert evidence['steps'] == [] and 'missing prebuilt input' in evidence['error']
    with pytest.raises(SystemExit, match='拒绝复用'):
        runner.main(['--c3', 'missing', '--output', str(output)])


def test_actual_missing_c3_is_rejected_before_test_collection(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(runner, 'capture', lambda *args: json.dumps({'versions': {'fastdb4py': '0.2.1'}}))
    with pytest.raises(runner.RunnerError, match='预建c3不存在'):
        runner.preflight(SimpleNamespace(python=sys.executable, c3=tmp_path / 'missing'), {})


def test_fresh_receipts_validate_and_bind_selected_c3(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from tools.local_rc import portable_matrix_receipt, typescript_receipt
    calls = []
    def portable(path: Path, *, expected_stage: str) -> dict:
        calls.append((path, expected_stage))
        return {'rows': [{'transport': 'relay', 'c3_sha256': 'actual'}]}
    def typescript(path: Path, *, expected_stage: str) -> dict:
        calls.append((path, expected_stage))
        return {'packages': {'c3_sha256': 'actual'}}
    monkeypatch.setattr(portable_matrix_receipt, 'load_and_validate_receipt', portable)
    monkeypatch.setattr(typescript_receipt, 'load_and_validate_receipt', typescript)
    runner.validate_receipts(tmp_path, 'actual')
    assert calls == [(tmp_path / 'portable.v1.json', 'development'), (tmp_path / 'typescript.v1.json', 'development')]
    with pytest.raises(runner.RunnerError, match='哈希'):
        runner.validate_receipts(tmp_path, 'another build')


def test_run_step_cancellation_retains_evidence(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    real_wait = subprocess.Popen.wait
    interrupted = False
    def wait(process: subprocess.Popen, *args: Any, **kwargs: Any) -> int:
        nonlocal interrupted
        if not interrupted:
            interrupted = True
            raise KeyboardInterrupt
        return real_wait(process, *args, **kwargs)
    monkeypatch.setattr(subprocess.Popen, 'wait', wait)
    record = runner.run_step([sys.executable, '-c', 'import time; time.sleep(20)'], tmp_path / 'cancel', os.environ.copy(), 10)
    assert record['status'] == 'cancelled'
    assert record['process_exited'] and not record['cleanup_confirmed']
    assert (tmp_path / 'cancel/step.json').is_file()


def test_parent_exit_with_live_descendant_is_not_success(tmp_path: Path) -> None:
    script = 'import subprocess, sys; subprocess.Popen([sys.executable, "-c", "import time; time.sleep(20)"])'
    record = runner.run_step([sys.executable, '-c', script], tmp_path / 'residue', os.environ.copy(), 5)
    assert record['exit_code'] == 0 and record['status'] == 'failed'
    assert not record['cleanup_confirmed'] and '进程组仍存活' in record['error']


def test_main_normalizes_fixture_path_and_preserves_venv_python_link(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv('C2_PORTABLE_MATRIX_FIXTURE_ROOT', 'fixtures')
    for key in ('PYTEST_ADDOPTS', 'PYTEST_PLUGINS', 'PYTEST_DISABLE_PLUGIN_AUTOLOAD'):
        monkeypatch.delenv(key, raising=False)
    selected = tmp_path / 'chosen venv/python'
    selected.parent.mkdir()
    selected.symlink_to(sys.executable)
    def probe(options: Any, environment: Any) -> None:
        assert options.python == str(selected)
        assert environment['C2_PORTABLE_MATRIX_FIXTURE_ROOT'] == str(tmp_path / 'fixtures')
        assert environment['CTWO_TEST_C3_BIN'] == str(tmp_path / 'prebuilt c3')
        raise runner.RunnerError('probe only')
    monkeypatch.setattr(runner, 'preflight', probe)
    assert runner.main(['--python', str(selected), '--c3', 'prebuilt c3', '--output', 'evidence']) == 1


def test_main_cancellation_has_130_exit_and_cancelled_receipt(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    for key in ('PYTEST_ADDOPTS', 'PYTEST_PLUGINS', 'PYTEST_DISABLE_PLUGIN_AUTOLOAD'):
        monkeypatch.delenv(key, raising=False)
    def probe(*args: Any) -> None:
        raise KeyboardInterrupt
    monkeypatch.setattr(runner, 'preflight', probe)
    output = tmp_path / 'cancelled'
    assert runner.main(['--c3', 'c3', '--output', str(output)]) == 130
    assert json.loads((output / 'run.json').read_text())['status'] == 'cancelled'
