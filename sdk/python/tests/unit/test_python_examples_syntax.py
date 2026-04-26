from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

import pytest


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[4]


def _python310() -> str | None:
    explicit = os.environ.get("C2_TEST_PYTHON310")
    if explicit:
        return explicit

    found = shutil.which("python3.10")
    if found:
        return found

    uv = shutil.which("uv")
    if uv is None:
        return None

    result = subprocess.run(
        [uv, "python", "find", "3.10"],
        capture_output=True,
        text=True,
        timeout=10,
        check=False,
    )
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


def test_python_examples_compile_on_minimum_supported_python():
    python310 = _python310()
    if python310 is None:
        pytest.skip("Python 3.10 is not available")

    root = _repo_root()
    script = """
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
for path in sorted(root.rglob("*.py")):
    compile(path.read_text(encoding="utf-8"), str(path), "exec")
"""
    result = subprocess.run(
        [python310, "-c", script, str(root / "examples/python")],
        cwd=root,
        capture_output=True,
        text=True,
        timeout=20,
        check=False,
    )

    assert result.returncode == 0, result.stderr
