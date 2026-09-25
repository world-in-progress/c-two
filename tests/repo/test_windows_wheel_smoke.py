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


def test_clean_environment_removes_the_core_sdk_library_directory(tmp_path, monkeypatch):
    smoke = load_smoke()
    sdk_lib = str(tmp_path / "sdk" / "lib")
    monkeypatch.setattr(smoke.os, "environ", {
        "FASTDB_PAYLOAD_SYSTEM_LIB_DIR": sdk_lib,
        "PATH": sdk_lib + os.pathsep + "/system/bin",
    })
    cleaned = smoke.clean_environment()
    assert cleaned["PATH"] == "/system/bin"
    assert "FASTDB_PAYLOAD_SYSTEM_LIB_DIR" not in cleaned


def test_standard_user_driver_forwards_source_identity(tmp_path):
    import json
    import subprocess
    import sys
    from pathlib import Path
    wrapper = (Path(__file__).resolve().parents[2] / "tools/ci/windows_standard_user.ps1").read_text()
    driver_source = wrapper.split("    @'\n", 1)[1].split("\n'@ | Set-Content", 1)[0]
    helper = tmp_path / "helper.py"
    receipt = tmp_path / "receipt.json"
    helper.write_text("import json, pathlib, sys\npathlib.Path(sys.argv[sys.argv.index('--receipt')+1]).write_text(json.dumps(sys.argv))\n")
    config = {"workspace": str(tmp_path), "staged": {"helper": str(helper), "fastdb": "fastdb.whl", "ctwo": "ctwo.whl", "cli": "c3-platform.exe"},
              "receipt": str(receipt), "source_sha": "a" * 40}
    config_path = tmp_path / "inputs.json"
    config_path.write_text(json.dumps(config))
    driver = tmp_path / "driver.py"
    driver.write_text(driver_source)
    environment = dict(os.environ, SystemRoot=str(tmp_path / "windows"))
    subprocess.run([sys.executable, "-I", str(driver), str(config_path)], check=True, env=environment)
    args = json.loads(receipt.read_text())
    assert args[args.index("--source-sha") + 1] == "a" * 40
    assert args[args.index("--c3") + 1] == "c3-platform.exe"


@pytest.mark.parametrize("status,expected", [("passed", 0), ("failed", 1)])
def test_standard_user_wrapper_sets_its_own_exit_code(tmp_path, status, expected):
    import shutil
    from pathlib import Path
    pwsh = shutil.which("pwsh")
    if not pwsh:
        pytest.skip("PowerShell is required for wrapper exit-code execution")
    source = (Path(__file__).resolve().parents[2] / "tools/ci/windows_standard_user.ps1").read_text()
    tail = source[source.rindex('Write-Output "Standard-user wheel consumer:'):]
    wrapper = tmp_path / "wrapper.ps1"
    wrapper.write_text('$report = @{ status = $args[0] }\n' + tail)
    caller = tmp_path / "caller.ps1"
    caller.write_text('param($Wrapper, $Status)\n$global:LASTEXITCODE = 71\n& $Wrapper $Status\nexit $LASTEXITCODE\n')
    result = subprocess.run([pwsh, "-NoLogo", "-NoProfile", "-NonInteractive", "-File", str(caller), str(wrapper), status],
                            capture_output=True, text=True)
    assert result.returncode == expected, result.stdout + result.stderr
