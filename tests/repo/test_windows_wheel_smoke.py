"""Exercise the installed-consumer verifier's real subprocess failure path."""
import importlib.util
from pathlib import Path
import subprocess
import sys

import pytest


def test_dead_host_broken_stdin_does_not_leave_relay_alive():
    path = Path(__file__).resolve().parents[2] / "tools/ci/windows_wheel_smoke.py"
    spec = importlib.util.spec_from_file_location("wheel_smoke", path)
    smoke = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(smoke)
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
