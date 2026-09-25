from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import sys
from types import SimpleNamespace


def _runner():
    path = Path(__file__).resolve().parents[2] / "tools/ci/windows_native.py"
    spec = importlib.util.spec_from_file_location("windows_native", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_failure_keeps_exit_code_and_does_not_prevent_next_gate(tmp_path):
    runner = _runner()
    arguments = {"cwd": tmp_path, "output": tmp_path, "environment": os.environ.copy()}
    failure = runner.run_step("fails", [sys.executable, "-c", "print('compiler evidence'); raise SystemExit(7)"], **arguments)
    success = runner.run_step("next", [sys.executable, "-c", "print('next gate ran')"], **arguments)
    assert failure["status"] == "failed" and failure["exit_code"] == 7
    assert "compiler evidence" in (tmp_path / "fails.log").read_text()
    assert success["status"] == "passed" and success["exit_code"] == 0


def test_timeout_records_failure_and_reaps_child(tmp_path):
    record = _runner().run_step(
        "timeout", [sys.executable, "-c", "import time; print('started', flush=True); time.sleep(60)"],
        cwd=tmp_path, output=tmp_path, environment=os.environ.copy(), timeout=0.3,
    )
    assert record["status"] == "timed_out"
    assert record["process_exited"] is True
    assert record["exit_code"] != 0
    assert "started" in (tmp_path / "timeout.log").read_text()


def test_failed_prerequisite_marks_scope_failed_and_runs_independent_gate(tmp_path, monkeypatch):
    runner = _runner()
    # Exercise the Windows evidence orchestration with real portable child
    # commands; this test does not claim to execute a Windows native backend.
    monkeypatch.setattr(runner, "os", SimpleNamespace(name="nt", environ=os.environ))
    monkeypatch.setattr(runner.platform, "machine", lambda: "AMD64")
    monkeypatch.setattr(runner, "ROOT", tmp_path)
    monkeypatch.setattr(runner, "capture", lambda command, cwd: {"exit_code": 0, "output": "a" * 40})
    monkeypatch.setattr(runner, "gates", lambda python, output, scope: [
        ("wheel", [sys.executable, "-c", "print('wheel built')"], ()),
        ("install", [sys.executable, "-c", "raise SystemExit(7)"], ()),
        ("dependent", [sys.executable, "-c", "raise AssertionError('must not run')"], ("wheel", "install")),
        ("independent", [sys.executable, "-c", "print('independent executed')"], ()),
    ])
    output = tmp_path / "evidence"
    result = runner.main([
        "--scope", "local-platform", "--output", str(output),
        "--expected-c-two-sha", "a" * 40, "--expected-fastdb-sha", "a" * 40,
    ])
    evidence = json.loads((output / "run-evidence.json").read_text())
    assert result == 1
    assert evidence["status"] == "failed"
    assert evidence["scope"] == "local-platform"
    assert evidence["applicable_gates"] == ["wheel", "install", "dependent", "independent"]
    assert [step["status"] for step in evidence["steps"]] == ["passed", "failed", "not_run", "passed"]
    assert evidence["steps"][1]["exit_code"] == 7
    assert not (output / "dependent.log").exists()
    assert "independent executed" in (output / "independent.log").read_text()


def test_full_scope_gate_inventory_preserves_receipt_order():
    runner = _runner()
    gates = runner.gates(sys.executable, Path("evidence"), runner.FULL_SCOPE)
    names = [name for name, _, _ in gates]
    assert len(names) == 22 and len(set(names)) == 22
    index = {name: position for position, name in enumerate(names)}
    # Deployable application gates keep their static source-mode link path.
    assert runner.SOURCE_MODE_GATES <= set(names)
    assert index["cli-build"] < index["cli-test"] < index["cli-artifact"]
    # Nothing may relink the CLI after its bytes are retained.
    for name, command, _ in gates[index["cli-artifact"]:]:
        arguments = [str(part) for part in command]
        assert not any("cli/Cargo.toml" in argument for argument in arguments), name
    dependencies = {name: list(required) for name, _, required in gates}
    assert dependencies["cli-artifact"] == ["cli-test"]
    assert set(dependencies["portable-tests"]) == {"python-build", "cli-artifact"}
    assert "cli-artifact" in dependencies["typescript-tests"]


def test_gate_environment_selects_system_mode_except_for_app_gates(tmp_path):
    runner = _runner()
    lib_dir = tmp_path / "coresdk" / "lib"
    lib_dir.mkdir(parents=True)
    base = {
        "FASTDB_PAYLOAD_LINK_MODE": "system",
        "FASTDB_PAYLOAD_SYSTEM_LIB_DIR": "/ambient/runner/lib",
        "PATH": "/usr/bin",
    }
    system = runner.gate_environment("core-test", base, lib_dir)
    assert system["FASTDB_PAYLOAD_LINK_MODE"] == "system"
    assert system["FASTDB_PAYLOAD_SYSTEM_LIB_DIR"] == str(lib_dir)
    assert system["PATH"].startswith(f"{lib_dir}{runner.os.pathsep}")
    for gate in runner.SOURCE_MODE_GATES:
        child = runner.gate_environment(gate, base, lib_dir)
        assert child["FASTDB_PAYLOAD_LINK_MODE"] == "source", gate
        assert "FASTDB_PAYLOAD_SYSTEM_LIB_DIR" not in child, gate
        assert child["PATH"] == "/usr/bin"
    assert runner.gate_environment("core-test", base, None) == base


def test_validate_system_lib_dir_requires_absolute_directory_with_library(tmp_path):
    runner = _runner()
    prepared = tmp_path / "coresdk" / "lib"
    prepared.mkdir(parents=True)
    (prepared / runner.platform_fastdb_library()).write_bytes(b"host library")
    resolved, problem = runner.validate_system_lib_dir(prepared)
    assert problem is None and resolved is not None
    relative = runner.validate_system_lib_dir(Path("coresdk/lib"))
    assert relative[0] is None and "absolute" in relative[1]
    empty = runner.validate_system_lib_dir(tmp_path / "missing")
    assert empty[0] is None and "missing" in empty[1]
    without_library = tmp_path / "empty-lib"
    without_library.mkdir()
    result = runner.validate_system_lib_dir(without_library)
    assert result[0] is None and "missing" in result[1]


def test_full_scope_requires_prepared_system_sdk(tmp_path, monkeypatch):
    runner = _runner()
    monkeypatch.setattr(runner, "os", SimpleNamespace(name="nt", environ=os.environ))
    monkeypatch.setattr(runner.platform, "machine", lambda: "AMD64")
    monkeypatch.setattr(runner, "ROOT", tmp_path)
    monkeypatch.setattr(runner, "capture", lambda command, cwd: {"exit_code": 0, "output": "a" * 40})
    monkeypatch.setattr(runner, "gates", lambda python, output, scope: [
        (name, [sys.executable, "-c", "print('gate')"], ()) for name in runner.SOURCE_MODE_GATES
    ])
    output = tmp_path / "evidence"
    result = runner.main([
        "--scope", "full", "--output", str(output),
        "--expected-c-two-sha", "a" * 40, "--expected-fastdb-sha", "a" * 40,
    ])
    evidence = json.loads((output / "run-evidence.json").read_text())
    assert result == 1
    assert evidence["status"] == "failed"
    assert "--fastdb-system-lib-dir" in evidence["error"]
    assert evidence["fastdb_sdk"]["system_lib_dir"] is None


def test_full_scope_rejects_sdk_without_the_platform_library(tmp_path, monkeypatch):
    runner = _runner()
    monkeypatch.setattr(runner, "os", SimpleNamespace(name="nt", environ=os.environ))
    monkeypatch.setattr(runner.platform, "machine", lambda: "AMD64")
    monkeypatch.setattr(runner, "ROOT", tmp_path)
    monkeypatch.setattr(runner, "capture", lambda command, cwd: {"exit_code": 0, "output": "a" * 40})
    monkeypatch.setattr(runner, "gates", lambda python, output, scope: [
        (name, [sys.executable, "-c", "print('gate')"], ()) for name in runner.SOURCE_MODE_GATES
    ])
    prepared = tmp_path / "coresdk" / "lib"
    prepared.mkdir(parents=True)
    output = tmp_path / "evidence"
    result = runner.main([
        "--scope", "full", "--output", str(output),
        "--expected-c-two-sha", "a" * 40, "--expected-fastdb-sha", "a" * 40,
        "--fastdb-system-lib-dir", str(prepared),
    ])
    evidence = json.loads((output / "run-evidence.json").read_text())
    assert result == 1
    assert "fastdb.lib" in evidence["error"]


def test_full_scope_children_receive_split_link_modes(tmp_path, monkeypatch):
    runner = _runner()
    monkeypatch.setattr(runner, "os", SimpleNamespace(name="nt", environ=os.environ))
    monkeypatch.setattr(runner.platform, "machine", lambda: "AMD64")
    monkeypatch.setattr(runner, "ROOT", tmp_path)
    monkeypatch.setattr(runner, "capture", lambda command, cwd: {"exit_code": 0, "output": "a" * 40})
    probe = "import os; print(os.environ.get('FASTDB_PAYLOAD_LINK_MODE')); print(os.environ.get('FASTDB_PAYLOAD_SYSTEM_LIB_DIR'))"
    stubbed = [("core-test", [sys.executable, "-c", probe], ())]
    stubbed += [
        (name, [sys.executable, "-c", probe if name == "cli-build" else "print('source gate')"], ())
        for name in sorted(runner.SOURCE_MODE_GATES)
    ]
    monkeypatch.setattr(runner, "gates", lambda python, output, scope: stubbed)
    prepared = tmp_path / "coresdk" / "lib"
    prepared.mkdir(parents=True)
    (prepared / "fastdb.lib").write_bytes(b"import library")
    output = tmp_path / "evidence"
    result = runner.main([
        "--scope", "full", "--output", str(output),
        "--expected-c-two-sha", "a" * 40, "--expected-fastdb-sha", "a" * 40,
        "--fastdb-system-lib-dir", str(prepared),
    ])
    assert result == 0
    core_log = (output / "core-test.log").read_text()
    assert core_log.splitlines()[:2] == ["system", str(prepared.resolve())]
    cli_log = (output / "cli-build.log").read_text()
    assert cli_log.splitlines()[:2] == ["source", "None"]


def test_full_scope_fails_when_source_mode_gate_names_disappear(tmp_path, monkeypatch):
    runner = _runner()
    monkeypatch.setattr(runner, "os", SimpleNamespace(name="nt", environ=os.environ))
    monkeypatch.setattr(runner.platform, "machine", lambda: "AMD64")
    monkeypatch.setattr(runner, "ROOT", tmp_path)
    monkeypatch.setattr(runner, "capture", lambda command, cwd: {"exit_code": 0, "output": "a" * 40})
    real_gates = runner.gates
    renamed = [
        (name if name != "cli-build" else "cli-compile", command, dependencies)
        for name, command, dependencies in real_gates(sys.executable, Path("evidence"), runner.FULL_SCOPE)
    ]
    monkeypatch.setattr(runner, "gates", lambda python, output, scope: renamed)
    prepared = tmp_path / "coresdk" / "lib"
    prepared.mkdir(parents=True)
    (prepared / "fastdb.lib").write_bytes(b"import library")
    output = tmp_path / "evidence"
    result = runner.main([
        "--scope", "full", "--output", str(output),
        "--expected-c-two-sha", "a" * 40, "--expected-fastdb-sha", "a" * 40,
        "--fastdb-system-lib-dir", str(prepared),
    ])
    evidence = json.loads((output / "run-evidence.json").read_text())
    assert result == 1
    assert "cli-build" in evidence["error"] and "no longer exist" in evidence["error"]
