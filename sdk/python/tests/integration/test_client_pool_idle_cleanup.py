"""Idle cache eviction must close OS sockets in a long-lived SDK client.

Windows runs the same retirement/EOF checks with LocalListener Named Pipes in
c2-ipc's real_idle_pool_* tests; portable per-process pipe counting is unavailable.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
from contextlib import contextmanager

import pytest

from tests.fixtures.process_control import readline_with_timeout


FIXTURES = Path(__file__).resolve().parents[1] / 'fixtures'


def _unix_socket_count(pid: int) -> int:
    if sys.platform == 'linux':
        # Inodes are used only to classify this snapshot's FDs as AF_UNIX.
        # No identity comparison across snapshots: kernels can reuse IDs.
        process = Path('/proc') / str(pid)
        unix_inodes = {
            line.split()[6]
            for line in (process / 'net/unix').read_text().splitlines()[1:]
        }
        count = 0
        for fd in (process / 'fd').iterdir():
            try:
                target = os.readlink(fd)
            except FileNotFoundError:  # A descriptor closed during enumeration.
                continue
            if target.startswith('socket:[') and target[8:-1] in unix_inodes:
                count += 1
        return count
    result = subprocess.run(
        ['lsof', '-nP', '-a', '-p', str(pid), '-U', '-Ff'],
        capture_output=True, text=True, timeout=5,
    )
    assert result.returncode == 0, f'lsof failed: {result.stderr}'
    return sum(line.startswith('f') for line in result.stdout.splitlines())


def _wait_count(pid: int, expected: int) -> None:
    deadline = time.monotonic() + 8
    observations = []
    while time.monotonic() < deadline:
        actual = _unix_socket_count(pid)
        observations.append(actual)
        if actual == expected:
            # Observe again after dispatch/EOF handling settles.
            time.sleep(0.2)
            if _unix_socket_count(pid) == expected:
                return
        time.sleep(0.05)
    pytest.fail(f'pid {pid}: expected {expected} Unix socket FDs, saw {observations}')


@contextmanager
def _process(script: str, env: dict[str, str], log: Path, *args: str):
    with log.open('w') as errors:
        process = subprocess.Popen(
            [sys.executable, '-u', str(FIXTURES / script), *args],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors,
            text=True, env=env,
        )
        try:
            yield process
        finally:
            if process.poll() is None:
                process.stdin.close()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            process.stdout.close()
            if not process.stdin.closed:
                process.stdin.close()


def _message(process: subprocess.Popen, log: Path, timeout: float = 15) -> dict:
    line = readline_with_timeout(process.stdout, timeout)
    assert line, f'process {process.pid} exited ({process.poll()}): {log.read_text()}'
    return json.loads(line)


@pytest.mark.skipif(
    sys.platform not in ('linux', 'darwin'),
    reason='OS socket counting needs Linux /proc or macOS lsof; Rust covers Named Pipes',
)
@pytest.mark.timeout(240)
def test_client_pool_idle_cleanup(tmp_path):
    if sys.platform == 'darwin':
        assert shutil.which('lsof'), 'macOS regression gate requires lsof'
    # Only child environments change; each test owns a private namespace,
    # so xdist needs no process-global environment lock.
    with tempfile.TemporaryDirectory(prefix='c2-idle-', dir='/tmp') as ipc_root:
        env = {**os.environ, 'C2_ENV_FILE': '', 'C2_RELAY_ANCHOR_ADDRESS': '',
               'C2_IPC_ROOT': ipc_root}
        server_log = tmp_path / 'server.stderr'
        client_log = tmp_path / 'client.stderr'
        with _process('idle_pool_server.py', env, server_log) as server:
            hello = _message(server, server_log)
            assert hello['pid'] == server.pid
            assert hello['address'].startswith('ipc://')
            baseline = _unix_socket_count(server.pid)
            with _process('idle_pool_client.py', env, client_log, hello['address']) as client:
                assert _message(client, client_log)['pid'] == client.pid
                for phase in ('calls', 'idle', 'idle'):
                    client.stdin.write(phase + '\n')
                    client.stdin.flush()
                    ack = _message(client, client_log, timeout=80)
                    assert ack['done'] == phase
                    if phase == 'idle':
                        assert ack['elapsed'] >= 62
                    # Exactly one accepted connection, after every idle round.
                    _wait_count(server.pid, baseline + 1)
                    assert client.poll() is None, client_log.read_text()
                client.stdin.write('quit\n')
                client.stdin.flush()
                assert client.wait(timeout=10) == 0, client_log.read_text()
            _wait_count(server.pid, baseline)
            server.stdin.close()
            assert server.wait(timeout=10) == 0, server_log.read_text()
