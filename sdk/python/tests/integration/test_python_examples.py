"""Integration checks for runnable Python examples."""
from __future__ import annotations

import os
import signal
import subprocess
import sys
import time
from pathlib import Path

import pytest
from c_two.transport.client.util import ping


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[4]


def _wait_for_stdout(proc: subprocess.Popen[str], text: str, timeout: float = 20.0) -> str:
    assert proc.stdout is not None
    deadline = time.monotonic() + timeout
    lines: list[str] = []
    while time.monotonic() < deadline:
        line = proc.stdout.readline()
        if line:
            lines.append(line)
            if text in line:
                return "".join(lines)
            continue
        if proc.poll() is not None:
            break
        time.sleep(0.1)
    raise AssertionError(
        f"Timed out waiting for {text!r}; output so far:\n{''.join(lines)}"
    )


def _extract_ipc_address(output: str) -> str:
    for token in output.replace(")", " ").split():
        if token.startswith("ipc://"):
            return token
    raise AssertionError(f"No ipc:// address found in output:\n{output}")


def _wait_for_ipc_ready(address: str, timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            if ping(address, timeout=0.5):
                return
        except Exception as exc:
            last_error = exc
        time.sleep(0.05)
    detail = f': {last_error}' if last_error is not None else ''
    raise AssertionError(f'Timed out waiting for IPC server {address}{detail}')


def _stop_process(proc: subprocess.Popen[str]) -> None:
    if proc.poll() is not None:
        return
    proc.send_signal(signal.SIGINT)
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def _example_env() -> dict[str, str]:
    env = os.environ.copy()
    env["C2_ENV_FILE"] = ""
    env["C2_RELAY_ANCHOR_ADDRESS"] = ""
    env["NO_PROXY"] = "127.0.0.1,localhost"
    env["no_proxy"] = "127.0.0.1,localhost"
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    env["PYTHONUNBUFFERED"] = "1"
    return env


def test_ipc_example():
    pytest.importorskip("pandas", reason="Python grid examples require examples dependencies")
    pytest.importorskip("pyarrow", reason="Python grid examples require examples dependencies")

    root = _repo_root()
    crm_proc: subprocess.Popen[str] | None = None
    try:
        crm_proc = subprocess.Popen(
            [
                sys.executable,
                str(root / "examples/python/ipc_resource.py"),
            ],
            cwd=root,
            env=_example_env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        output = _wait_for_stdout(crm_proc, "Grid CRM registered")
        ipc_address = _extract_ipc_address(output)
        _wait_for_ipc_ready(ipc_address)

        client = subprocess.run(
            [
                sys.executable,
                str(root / "examples/python/ipc_client.py"),
                ipc_address,
            ],
            cwd=root,
            env={
                **_example_env(),
                "C2_RELAY_ANCHOR_ADDRESS": "http://127.0.0.1:1",
            },
            capture_output=True,
            text=True,
            timeout=20,
            check=False,
        )

        assert client.returncode == 0, client.stderr
        assert "Connected (mode:" in client.stdout
        assert "IPC client done." in client.stdout
    finally:
        if crm_proc is not None:
            _stop_process(crm_proc)


def test_relay_example(start_c3_relay):
    pytest.importorskip("pandas", reason="Python grid examples require examples dependencies")
    pytest.importorskip("pyarrow", reason="Python grid examples require examples dependencies")

    root = _repo_root()
    relay = start_c3_relay()
    relay_url = relay.url

    crm_proc: subprocess.Popen[str] | None = None
    try:
        crm_proc = subprocess.Popen(
            [
                sys.executable,
                str(root / "examples/python/relay_resource.py"),
                "--relay-url",
                relay_url,
            ],
            cwd=root,
            env=_example_env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        _wait_for_stdout(crm_proc, "Grid CRM registered")

        client = subprocess.run(
            [
                sys.executable,
                str(root / "examples/python/relay_client.py"),
                "--relay-url",
                relay_url,
            ],
            cwd=root,
            env=_example_env(),
            capture_output=True,
            text=True,
            timeout=20,
            check=False,
        )

        assert client.returncode == 0, client.stderr
        assert "[Client] Done." in client.stdout
    finally:
        if crm_proc is not None:
            _stop_process(crm_proc)


def test_fastdb_relay_example(start_c3_relay):
    pytest.importorskip("fastdb4py", reason="FastDB grid examples require fastdb4py")
    pytest.importorskip("pandas", reason="Python grid examples require examples dependencies")
    pytest.importorskip("pyarrow", reason="Python grid examples require examples dependencies")

    root = _repo_root()
    relay = start_c3_relay()
    relay_url = relay.url

    crm_proc: subprocess.Popen[str] | None = None
    try:
        crm_proc = subprocess.Popen(
            [
                sys.executable,
                str(root / "examples/python/fastdb_relay_resource.py"),
                "--relay-url",
                relay_url,
            ],
            cwd=root,
            env=_example_env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        _wait_for_stdout(crm_proc, "FastDB Grid CRM registered")

        client = subprocess.run(
            [
                sys.executable,
                str(root / "examples/python/fastdb_relay_client.py"),
                "--relay-url",
                relay_url,
            ],
            cwd=root,
            env=_example_env(),
            capture_output=True,
            text=True,
            timeout=20,
            check=False,
        )

        assert client.returncode == 0, client.stderr
        assert "[FastDB Client] Done." in client.stdout
        assert "Held active IDs" in client.stdout
    finally:
        if crm_proc is not None:
            _stop_process(crm_proc)


def test_ipc_client_requires_address():
    root = _repo_root()
    client = subprocess.run(
        [
            sys.executable,
            str(root / "examples/python/ipc_client.py"),
        ],
        cwd=root,
        env=_example_env(),
        capture_output=True,
        text=True,
        timeout=20,
        check=False,
    )

    assert client.returncode == 2
    assert "address" in client.stderr
