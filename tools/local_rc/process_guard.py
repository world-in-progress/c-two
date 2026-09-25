"""Process lifetime helpers for real-call local-candidate evidence."""

from __future__ import annotations

from contextlib import contextmanager
import os
from pathlib import Path
import signal
import subprocess
import time
from typing import IO, Iterator, Mapping, Sequence


def stop_process_tree(
    process: subprocess.Popen[object],
    *,
    timeout: float = 5.0,
) -> None:
    """Stop a candidate child and its process group, escalating if required."""

    if process.poll() is not None:
        return
    if os.name == "posix":
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
    else:
        process.terminate()
    deadline = time.monotonic() + timeout
    while process.poll() is None and time.monotonic() < deadline:
        time.sleep(0.02)
    if process.poll() is not None:
        return
    if os.name == "posix":
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            return
    else:
        process.kill()
    process.wait(timeout=max(timeout, 1.0))


@contextmanager
def guarded_process(
    command: Sequence[str],
    *,
    cwd: Path,
    env: Mapping[str, str] | None = None,
    stdin: int | IO[bytes] | None = None,
    stdout: int | IO[bytes] | None = None,
    stderr: int | IO[bytes] | None = None,
) -> Iterator[subprocess.Popen[bytes]]:
    """Start a child in a dedicated process group and always drain it."""

    process = subprocess.Popen(
        list(command),
        cwd=cwd,
        env=None if env is None else dict(env),
        stdin=stdin,
        stdout=stdout,
        stderr=stderr,
        start_new_session=os.name == "posix",
    )
    try:
        yield process
    finally:
        stop_process_tree(process)
