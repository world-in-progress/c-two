from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[4]


def _parse_args(
    script_name: str,
    argv: list[str],
    *,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    root = _repo_root()
    script = f"""
import importlib.util
import pathlib
import sys

path = pathlib.Path({str(root / "examples/python")!r}) / {script_name!r}
sys.path.insert(0, str(path.parent))
sys.argv = [str(path), *{argv!r}]
spec = importlib.util.spec_from_file_location("example_module", path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
args = module.parse_args()
for key, value in vars(args).items():
    print(f"{{key}}={{value}}")
"""
    child_env = {
        k: v
        for k, v in os.environ.items()
        if k not in {"C2_ENV_FILE", "C2_RELAY_ANCHOR_ADDRESS"}
    }
    child_env["C2_ENV_FILE"] = ""
    child_env.update(env or {})
    return subprocess.run(
        [sys.executable, "-c", script],
        cwd=root,
        env=child_env,
        capture_output=True,
        text=True,
        timeout=20,
        check=False,
    )


def _python310() -> str | None:
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


def test_python_sdk_does_not_export_embedded_native_relay():
    import importlib.util
    import c_two._native as native

    assert not hasattr(native, "NativeRelay")
    assert importlib.util.find_spec("c_two.relay") is None


def test_ipc_client_requires_address():
    result = _parse_args("ipc_client.py", [])

    assert result.returncode == 2
    assert "address" in result.stderr


def test_ipc_client_uses_given_address(tmp_path):
    env_file = tmp_path / ".env"
    env_file.write_text("C2_RELAY_ANCHOR_ADDRESS=http://127.0.0.1:9137\n", encoding="utf-8")

    result = _parse_args(
        "ipc_client.py",
        ["ipc://manual-address"],
        env={
            "C2_ENV_FILE": str(env_file),
            "C2_RELAY_ANCHOR_ADDRESS": "http://127.0.0.1:9140",
        },
    )

    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "address=ipc://manual-address"


def test_ipc_client_rejects_http():
    result = _parse_args("ipc_client.py", ["http://127.0.0.1:8300"])

    assert result.returncode == 2
    assert "ipc://" in result.stderr


def test_relay_client_cli_wins(tmp_path):
    env_file = tmp_path / ".env"
    env_file.write_text("C2_RELAY_ANCHOR_ADDRESS=http://127.0.0.1:9137\n", encoding="utf-8")

    result = _parse_args(
        "relay_client.py",
        ["--relay-url", "http://127.0.0.1:9140"],
        env={"C2_ENV_FILE": str(env_file)},
    )

    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "relay_url=http://127.0.0.1:9140"


def test_relay_client_rejects_ipc():
    result = _parse_args("relay_client.py", ["--relay-url", "ipc://not-a-relay"])

    assert result.returncode == 2
    assert "http://" in result.stderr


def test_relay_client_default():
    result = _parse_args("relay_client.py", [])

    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "relay_url=http://127.0.0.1:8300"


def test_relay_client_env_file(tmp_path):
    env_file = tmp_path / ".env"
    env_file.write_text("C2_RELAY_ANCHOR_ADDRESS=http://127.0.0.1:9137\n", encoding="utf-8")

    result = _parse_args(
        "relay_client.py",
        [],
        env={"C2_ENV_FILE": str(env_file)},
    )

    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "relay_url=http://127.0.0.1:9137"
