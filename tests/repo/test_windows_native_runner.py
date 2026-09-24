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
