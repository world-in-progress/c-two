"""Idle cache eviction must close OS sockets in a long-lived SDK client.

Windows runs the same retirement/EOF checks with LocalListener Named Pipes in
c2-ipc's real_idle_pool_* tests; portable per-process pipe counting is unavailable.
"""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from contextlib import contextmanager

import pytest
import c_two as cc
import c_two.endpoint

from tests.fixtures.process_control import readline_with_timeout


FIXTURES = Path(__file__).resolve().parents[1] / 'fixtures'


def _unix_socket_count(pid: int, endpoint_name: str | None = None) -> int:
    if sys.platform == 'linux':
        # Inodes are used only to classify this snapshot's FDs as AF_UNIX.
        # No identity comparison across snapshots: kernels can reuse IDs.
        process = Path('/proc') / str(pid)
        unix_inodes = set()
        for line in (process / 'net/unix').read_text().splitlines()[1:]:
            fields = line.split()
            if endpoint_name is None or (
                len(fields) == 8 and Path(fields[7]).name == endpoint_name
            ):
                unix_inodes.add(fields[6])
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
        ['lsof', '-nP', '-a', '-p', str(pid), '-U', '-Ffn'],
        capture_output=True, text=True, timeout=5,
    )
    assert result.returncode == 0, f'lsof failed: {result.stderr}'
    if endpoint_name is not None:
        return sum(
            line.startswith('n') and Path(line[1:]).name == endpoint_name
            for line in result.stdout.splitlines()
        )
    return sum(line.startswith('f') for line in result.stdout.splitlines())


def _wait_count(pid: int, expected: int, endpoint_name: str | None = None) -> None:
    deadline = time.monotonic() + 8
    observations = []
    while time.monotonic() < deadline:
        actual = _unix_socket_count(pid, endpoint_name)
        observations.append(actual)
        if actual == expected:
            # Observe again after dispatch/EOF handling settles.
            time.sleep(0.2)
            if _unix_socket_count(pid, endpoint_name) == expected:
                return
        time.sleep(0.05)
    pytest.fail(
        f'pid {pid}, endpoint {endpoint_name}: expected {expected} Unix socket FDs, '
        f'saw {observations}'
    )


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


@pytest.mark.platform('posix')
@pytest.mark.timeout(240)
def test_client_pool_idle_cleanup(tmp_path):
    if sys.platform == 'darwin':
        assert shutil.which('lsof'), 'macOS regression gate requires lsof'
    # Only child environments change; each test owns a private namespace,
    # so xdist needs no process-global environment lock.
    with tempfile.TemporaryDirectory(prefix='c2-idle-', dir='/tmp') as ipc_root:
        # A peer EOF must remain counted until the accepted FD itself closes.
        endpoint = Path(ipc_root) / 'fd-probe'
        with socket.socket(socket.AF_UNIX) as listener:
            listener.bind(str(endpoint))
            listener.listen()
            assert _unix_socket_count(os.getpid(), endpoint.name) == 1
            with socket.socket(socket.AF_UNIX) as client:
                client.connect(str(endpoint))
                accepted, _ = listener.accept()
                with accepted:
                    assert _unix_socket_count(os.getpid(), endpoint.name) == 2
                    client.close()
                    assert accepted.recv(1) == b''
                    assert _unix_socket_count(os.getpid(), endpoint.name) == 2
            assert _unix_socket_count(os.getpid(), endpoint.name) == 1
        endpoint.unlink()
        env = {**os.environ, 'C2_ENV_FILE': '', 'C2_RELAY_ANCHOR_ADDRESS': '',
               'C2_IPC_ROOT': ipc_root}
        server_log = tmp_path / 'server.stderr'
        client_log = tmp_path / 'client.stderr'
        with _process('idle_pool_server.py', env, server_log) as server:
            hello = _message(server, server_log)
            assert hello['pid'] == server.pid
            assert hello['address'].startswith('ipc://')
            endpoint_name = Path(cc.endpoint.local_endpoint_context(root=ipc_root).endpoint_name(
                hello['address'],
            )).name
            # READY follows PONG, before the server necessarily drops that
            # probe's accepted FD. Wait for only the listener before sampling
            # the full-process baseline; later leak assertions remain unfiltered.
            _wait_count(server.pid, 1, endpoint_name)
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
        assert not list(Path(ipc_root).glob('*.lease'))
        assert not any(path.is_socket() for path in Path(ipc_root).iterdir())
