"""Graceful console interruption for Python server subprocesses."""
from __future__ import annotations

import os
import signal
import queue
import shutil
from pathlib import Path
import threading
from typing import TextIO
import subprocess


def python_process_options() -> dict[str, int]:
    # CTRL_BREAK_EVENT targets a process group instead of the runner console.
    return {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == "nt" else {}


def interrupt_python_process(process: subprocess.Popen[str]) -> None:
    process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)


def readline_with_timeout(stream: TextIO, timeout: float) -> str:
    """Read one readiness line from a subprocess pipe on every platform."""
    result: queue.Queue[tuple[str | None, BaseException | None]] = queue.Queue(maxsize=1)

    def read() -> None:
        try:
            result.put((stream.readline(), None))
        except Exception as error:
            result.put((None, error))

    threading.Thread(target=read, daemon=True).start()
    try:
        line, error = result.get(timeout=timeout)
    except queue.Empty:
        raise TimeoutError("timed out waiting for subprocess readiness") from None
    if error is not None:
        raise error
    assert line is not None
    return line


def portable_command(command: list[str]) -> list[str]:
    """Invoke npm's JS entrypoint instead of relying on Windows .cmd execution."""
    if command[0] != "npm" or os.name != "nt":
        return command
    node = shutil.which("node")
    npm = shutil.which("npm")
    if node is None:
        raise RuntimeError("Node.js is required to run npm")
    candidates = []
    if os.environ.get("npm_execpath"):
        candidates.append(Path(os.environ["npm_execpath"]))
    candidates.append(Path(node).parent / "node_modules/npm/bin/npm-cli.js")
    if npm:
        candidates.append(Path(npm).parent / "node_modules/npm/bin/npm-cli.js")
    for candidate in candidates:
        if candidate.is_file():
            return [node, str(candidate), *command[1:]]
    raise RuntimeError("npm-cli.js is required from the Node.js installation")
