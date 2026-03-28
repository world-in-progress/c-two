"""Integration tests for ``c3 relay`` CLI command.

These tests start a real relay server subprocess and verify it's functional.
"""
from __future__ import annotations

import json
import subprocess
import sys
import time
import urllib.request

import pytest

_RELAY_PORT = 18932  # Unlikely to collide with anything.
_RELAY_BIND = f'127.0.0.1:{_RELAY_PORT}'
_RELAY_URL = f'http://{_RELAY_BIND}'


def _wait_for_relay(url: str, timeout: float = 10.0) -> bool:
    """Poll the relay health endpoint until it responds or timeout."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            req = urllib.request.Request(f'{url}/health', method='GET')
            with urllib.request.urlopen(req, timeout=2) as resp:
                if resp.status == 200:
                    return True
        except Exception:
            pass
        time.sleep(0.2)
    return False


def _stop_relay(proc: subprocess.Popen) -> None:
    """Terminate relay subprocess gracefully, then force-kill."""
    import signal as _signal
    proc.send_signal(_signal.SIGINT)
    try:
        proc.wait(timeout=5.0)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=3.0)


_RELAY_SCRIPT = (
    'import sys; sys.argv = ["c3", "relay", "-b", "{bind}", "-w", "4", "-l", "WARNING"]; '
    'from c_two.cli import cli; cli(standalone_mode=False)'
)


class TestRelayLifecycle:
    """Start relay, verify endpoints, then stop."""

    @pytest.fixture(autouse=True)
    def relay_process(self):
        """Start a relay subprocess for each test."""
        script = _RELAY_SCRIPT.format(bind=_RELAY_BIND)
        proc = subprocess.Popen(
            [sys.executable, '-c', script],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        reachable = _wait_for_relay(_RELAY_URL)
        if not reachable:
            _stop_relay(proc)
            pytest.fail('Relay did not start in time')

        yield proc

        _stop_relay(proc)

    def test_health(self, relay_process):
        """GET /health returns 200 with JSON."""
        req = urllib.request.Request(f'{_RELAY_URL}/health', method='GET')
        with urllib.request.urlopen(req, timeout=3) as resp:
            assert resp.status == 200
            body = json.loads(resp.read())
            assert body['status'] == 'ok'

    def test_routes_empty(self, relay_process):
        """GET /_routes returns empty list when no upstreams registered."""
        req = urllib.request.Request(f'{_RELAY_URL}/_routes', method='GET')
        with urllib.request.urlopen(req, timeout=3) as resp:
            assert resp.status == 200
            body = json.loads(resp.read())
            assert body['routes'] == []

    def test_register_invalid_json(self, relay_process):
        """POST /_register with bad body returns 400."""
        req = urllib.request.Request(
            f'{_RELAY_URL}/_register',
            data=b'not json',
            method='POST',
            headers={'Content-Type': 'application/json'},
        )
        try:
            urllib.request.urlopen(req, timeout=3)
            pytest.fail('Expected HTTP error')
        except urllib.error.HTTPError as exc:
            assert exc.code == 400


class TestRelayEntryPoint:
    """Test that the CLI entry point works."""

    def test_help_via_python(self):
        script = (
            'import sys; sys.argv = ["c3", "relay", "--help"]; '
            'from c_two.cli import cli; cli()'
        )
        result = subprocess.run(
            [sys.executable, '-c', script],
            capture_output=True, text=True, timeout=10,
        )
        assert result.returncode == 0
        assert 'Start the C-Two HTTP relay server' in result.stdout
