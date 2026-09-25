"""Exercise the installed-consumer verifier's real subprocess failure path."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys

import pytest


def load_smoke():
    path = Path(__file__).resolve().parents[2] / "tools/ci/windows_wheel_smoke.py"
    spec = importlib.util.spec_from_file_location("wheel_smoke", path)
    smoke = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(smoke)
    return smoke


def test_dead_host_broken_stdin_does_not_leave_relay_alive():
    smoke = load_smoke()
    host = subprocess.Popen([sys.executable, "-c", "pass"], stdin=subprocess.PIPE, text=True)
    relay = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
    try:
        host.wait(timeout=10)
        with pytest.raises(OSError):
            host.stdin.write("stop\n")
            host.stdin.flush()
        cleanup = smoke.cleanup_processes(host, relay)
        assert cleanup["processes_exited"] is True
        assert relay.poll() is not None
        assert host.stdin.closed
    finally:
        for child in (host, relay):
            if child.poll() is None:
                child.kill()
            child.wait(timeout=10)


def test_clean_environment_strips_fastdb_sdk_without_touching_other_paths(tmp_path, monkeypatch):
    smoke = load_smoke()
    sdk = tmp_path / "fastdb-sdk"
    (sdk / "bin").mkdir(parents=True)
    system_python = "/opt/toolchain/python/bin"
    system_bins = "/usr/local/bin:/usr/bin:/bin"
    environment = {
        "PATH": os.pathsep.join([str(sdk / "bin"), system_python, system_bins]),
        "FASTDB_SDK": str(sdk),
        "FASTDB_SDK_VERSION": "0.2.1",
        "DYLD_FALLBACK_LIBRARY_PATH": str(sdk / "lib"),
        "LD_LIBRARY_PATH": str(sdk / "lib"),
        "C2_RELAY_ANCHOR_ADDRESS": "http://relay.example:8080",
        "VIRTUAL_ENV": "/somewhere/else",
        "HOME": "/home/runner",
        "SYSTEMROOT": r"C:\Windows",
    }
    monkeypatch.setattr(smoke.os, "environ", environment)
    cleaned = smoke.clean_environment()
    assert "FASTDB_SDK" not in cleaned
    assert "FASTDB_SDK_VERSION" not in cleaned
    assert "DYLD_FALLBACK_LIBRARY_PATH" not in cleaned
    assert "LD_LIBRARY_PATH" not in cleaned
    # The relay anchor is neutralized to an explicit empty value, not removed.
    assert cleaned["C2_RELAY_ANCHOR_ADDRESS"] == ""
    assert "VIRTUAL_ENV" not in cleaned
    # Required OS/Python PATH entries and unrelated variables survive.
    path_entries = cleaned["PATH"].split(os.pathsep)
    assert str(sdk / "bin") not in path_entries
    assert system_python in path_entries
    assert cleaned["HOME"] == "/home/runner"
    assert cleaned["SYSTEMROOT"] == r"C:\Windows"
    assert cleaned["C2_ENV_FILE"] == "" and cleaned["PYTHONUTF8"] == "1"


def test_clean_environment_keeps_path_when_no_fastdb_sdk_is_exported(monkeypatch, tmp_path):
    smoke = load_smoke()
    environment = {"PATH": "/usr/local/bin:/usr/bin", "HOME": str(tmp_path)}
    monkeypatch.setattr(smoke.os, "environ", environment)
    cleaned = smoke.clean_environment()
    assert cleaned["PATH"] == "/usr/local/bin:/usr/bin"
    assert cleaned["HOME"] == str(tmp_path)
