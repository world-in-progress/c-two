"""Integration tests for ``cc.serve()`` — subprocess lifecycle.

Tests start a real CRM resource process that calls ``cc.serve()``,
then send SIGINT and verify graceful shutdown.
"""
from __future__ import annotations

import os
import signal as _signal
import subprocess
import sys
import textwrap
import time

import pytest


_SERVE_SCRIPT = textwrap.dedent("""\
    import c_two as cc
    import os, sys

    @cc.icrm(namespace='cc.test.serve_integ', version='0.1.0')
    class IPing:
        def ping(self) -> str:
            ...

        @cc.on_shutdown
        def cleanup(self):
            ...

    class Ping:
        def __init__(self):
            self._cleaned = False

        def cleanup(self):
            # Write a marker file so the test can verify shutdown ran.
            marker = os.environ.get('SHUTDOWN_MARKER', '')
            if marker:
                with open(marker, 'w') as f:
                    f.write('cleaned')
            self._cleaned = True

        def ping(self) -> str:
            return 'pong'

    cc.register(IPing, Ping(), name='ping')
    cc.serve()
""")


def _wait_for_output(proc: subprocess.Popen, pattern: str, timeout: float = 10.0) -> bool:
    """Wait until a pattern appears in the process stdout."""
    deadline = time.monotonic() + timeout
    buf = ''
    while time.monotonic() < deadline:
        line = proc.stdout.readline()
        if line:
            buf += line
            if pattern in buf:
                return True
    return False


class TestServeSubprocess:
    """Start a serve() subprocess, verify it blocks, send SIGINT."""

    def test_serve_blocks_and_sigint_shuts_down(self, tmp_path):
        """Process blocks on cc.serve(), SIGINT triggers graceful exit."""
        marker = tmp_path / 'shutdown_marker.txt'

        env = os.environ.copy()
        env['SHUTDOWN_MARKER'] = str(marker)
        env.pop('C2_RELAY_ADDRESS', None)
        env['PYTHONUNBUFFERED'] = '1'

        proc = subprocess.Popen(
            [sys.executable, '-c', _SERVE_SCRIPT],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
            cwd=str(tmp_path),  # Avoid picking up .env from repo root.
        )

        # Wait for the banner to appear (proves serve() is blocking).
        ready = _wait_for_output(proc, 'Serving')
        if not ready:
            proc.kill()
            proc.wait()
            stderr = proc.stderr.read() if proc.stderr else ''
            pytest.fail(f'serve() banner did not appear. stderr:\n{stderr}')

        # Process should still be alive (blocking on serve).
        assert proc.poll() is None

        # Send SIGINT to trigger graceful shutdown.
        proc.send_signal(_signal.SIGINT)

        try:
            proc.wait(timeout=10.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
            pytest.fail('Process did not exit after SIGINT')

        assert proc.returncode == 0

        # Verify @on_shutdown was invoked.
        assert marker.exists(), '@on_shutdown marker file not created'
        assert marker.read_text() == 'cleaned'

    def test_serve_no_crm_still_blocks(self):
        """cc.serve() with no CRM registered still blocks (no crash)."""
        script = textwrap.dedent("""\
            import c_two as cc
            cc.serve()
        """)

        env = os.environ.copy()
        env.pop('C2_RELAY_ADDRESS', None)
        env['PYTHONUNBUFFERED'] = '1'

        proc = subprocess.Popen(
            [sys.executable, '-c', script],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
            cwd=str(os.path.dirname(__file__)),  # Avoid .env from repo root.
        )

        # Wait for the "no CRM" banner.
        ready = _wait_for_output(proc, 'Serving 0')
        if not ready:
            proc.kill()
            proc.wait()
            pytest.fail('serve() banner did not appear')

        # Process is alive.
        assert proc.poll() is None

        # Stop it.
        proc.send_signal(_signal.SIGINT)
        try:
            proc.wait(timeout=5.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()

        assert proc.returncode == 0
