from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import sys


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
