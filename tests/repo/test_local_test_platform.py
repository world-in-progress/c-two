"""Real process ownership and stable resource coordination for local testing."""
import importlib
import os
from pathlib import Path
import subprocess
import sys
import time
import shutil

import pytest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))


def backend():
    assert importlib.util.find_spec('tools.dev.test_platform') is not None, 'platform backend is missing'
    return importlib.import_module('tools.dev.test_platform')


def test_normal_exit_has_no_owned_processes(tmp_path):
    platform = backend()
    with (tmp_path / 'log').open('wb') as log:
        tree = platform.ProcessTree([sys.executable, '-c', 'print("ok")'], ROOT, os.environ.copy(), log)
        try:
            assert tree.process.wait(timeout=5) == 0
            assert not tree.alive()
        finally:
            tree.close()


def test_stop_cleans_orphan_grandchild_without_touching_unrelated_process(tmp_path):
    platform = backend()
    marker = tmp_path / 'grandchild'
    grandchild = f'from pathlib import Path; import time; Path({str(marker)!r}).write_text("ready"); time.sleep(60)'
    child = f'import subprocess,sys; subprocess.Popen([sys.executable,"-c",{grandchild!r}])'
    parent = f'import subprocess,sys; subprocess.Popen([sys.executable,"-c",{child!r}])'
    unrelated = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
    with (tmp_path / 'log').open('wb') as log:
        tree = platform.ProcessTree([sys.executable, '-c', parent], ROOT, os.environ.copy(), log)
        try:
            assert tree.process.wait(timeout=5) == 0
            deadline = time.monotonic() + 5
            while not marker.exists() and time.monotonic() < deadline:
                time.sleep(.02)
            assert marker.exists()
            assert tree.alive()
            tree.stop()
            assert not tree.alive()
            assert unrelated.poll() is None
        finally:
            tree.close()
            unrelated.kill()
            unrelated.wait(timeout=5)


def test_close_stops_a_live_tree(tmp_path):
    platform = backend()
    with (tmp_path / 'log').open('wb') as log:
        tree = platform.ProcessTree([sys.executable, '-c', 'import time; time.sleep(60)'], ROOT, os.environ.copy(), log)
        tree.close()
        assert tree.process.poll() is not None
        assert not tree.alive()
        tree.close()


@pytest.mark.skipif(os.name != 'nt', reason='Windows Job process image diagnostics')
def test_owned_process_diagnostics_name_scoped_processes(tmp_path):
    platform = backend()
    with (tmp_path / 'log').open('wb') as log:
        tree = platform.ProcessTree([sys.executable, '-c', 'import time; time.sleep(60)'], ROOT, os.environ.copy(), log)
        try:
            rows = tree.diagnostics()
            assert any(row['pid'] == tree.process.pid and Path(row['image']).name.lower() == Path(sys.executable).name.lower() for row in rows)
        finally:
            tree.close()
        assert tree.process.poll() is not None
        assert not tree.alive()
        tree.close()


@pytest.mark.skipif(os.name != 'nt', reason='Windows MSVC scoped helper lifecycle')
@pytest.mark.parametrize('live_parent,foreign_path,unknown_child', [(False, False, False), (True, False, False), (False, True, False), (False, False, True)])
def test_compiler_helper_cleanup_requires_exact_scope(tmp_path, monkeypatch, live_parent, foreign_path, unknown_child):
    platform = backend()
    compiler_dir = tmp_path / 'selected compiler'
    compiler_dir.mkdir()
    compiler = compiler_dir / 'cl.exe'
    compiler.write_bytes(b'compiler')
    helper = compiler_dir / 'vctip.exe'
    # cmd.exe can be relocated and provides a real long-running child with
    # the expected image name, without borrowing any existing compiler daemon.
    shutil.copyfile(Path(os.environ['SystemRoot']) / 'System32/cmd.exe', helper)
    launched = helper
    if foreign_path:
        launched = tmp_path / 'vctip.exe'
        shutil.copyfile(helper, launched)
    monkeypatch.setattr(shutil, 'which', lambda name, *, path=None: str(compiler))
    marker = tmp_path / 'ready'
    code = ('import subprocess,time\nfrom pathlib import Path\n'
            f'subprocess.Popen([{str(launched)!r},"/d","/c","for /l %n in (0,0,1) do @rem"])\n'
            + ('subprocess.Popen([__import__("sys").executable,"-c","import time; time.sleep(60)"])\n' if unknown_child else '')
            + f'Path({str(marker)!r}).write_text("ready")\n'
            + ('time.sleep(60)\n' if live_parent else ''))
    with (tmp_path / 'log').open('wb') as log:
        tree = platform.ProcessTree([sys.executable, '-c', code], ROOT, os.environ.copy(), log)
        try:
            deadline = time.monotonic() + 5
            while not marker.exists() and time.monotonic() < deadline:
                time.sleep(.02)
            assert marker.exists()
            if not live_parent:
                tree.process.wait(timeout=5)
            rows = tree.reap_compiler_helpers(os.environ.copy())
            if live_parent or foreign_path or unknown_child:
                assert rows == [] and tree.alive()
            else:
                assert rows and all(Path(row['image']).resolve() == helper.resolve() for row in rows)
                assert not tree.alive()
        finally:
            tree.close()


def test_resource_lease_rejects_competing_process_and_releases(tmp_path):
    platform = backend()
    code = ('from pathlib import Path\nfrom tools.dev.test_platform import resource_lease, PlatformError\n'
            f'try:\n    with resource_lease(Path({str(tmp_path)!r})):\n        raise SystemExit(9)\n'
            'except PlatformError:\n    print("rejected")\n')
    env = {**os.environ, 'PYTHONPATH': str(ROOT), 'TMP': str(tmp_path)}
    with platform.resource_lease(tmp_path):
        result = subprocess.run([sys.executable, '-c', code], env=env, capture_output=True, text=True, timeout=5)
        assert result.returncode == 0, result.stdout + result.stderr
        assert 'rejected' in result.stdout
    with platform.resource_lease(tmp_path):
        pass


@pytest.mark.parametrize('version,napi,accepted', [('v22.11.0', '9', False), ('v22.16.0', '10', True), ('v24.0.0', '10', False), ('v22.16.0', 'invalid', False)])
def test_node_runtime_contract(version, napi, accepted):
    platform = backend()
    if accepted:
        platform.validate_node_runtime(version, napi)
    else:
        with pytest.raises(platform.PlatformError):
            platform.validate_node_runtime(version, napi)


@pytest.mark.parametrize('name,expected', [('win32', 'fastdb.dll'), ('darwin', 'libfastdb.dylib'), ('linux', 'libfastdb.so')])
def test_core_library_name(name, expected):
    assert backend().core_library_name(name) == expected
