"""Integration tests for server-side heartbeat detection."""
from __future__ import annotations

import os
import subprocess
import sys
import threading
import time

import pytest

import c_two as cc
from c_two.transport import Server
from c_two.transport.client.util import _endpoint_name_from_address, ping

from tests.fixtures.hello import HelloImpl
from tests.fixtures.ihello import Hello


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_counter = 0
_lock = threading.Lock()


def _unique_region(prefix: str = 'test_hb') -> str:
    global _counter
    with _lock:
        _counter += 1
        return f'{prefix}_{os.getpid()}_{_counter}'


def _wait_for_server(address: str, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if ping(address, timeout=0.5):
                return
        except Exception:
            pass
        time.sleep(0.05)
    raise TimeoutError(f'Server at {address} not ready after {timeout}s')


def _hello_server(address: str, *, ipc_overrides: dict[str, object]) -> Server:
    server = Server(bind_address=address, ipc_overrides=ipc_overrides)
    server.register_crm(Hello, HelloImpl(), name='hello')
    return server


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


class TestHeartbeatIntegration:
    """End-to-end heartbeat tests with real Server via SOTA API."""

    def test_active_connection_survives_heartbeat(self):
        """An active client is NOT disconnected by heartbeat probes."""
        address = f'ipc://{_unique_region()}'
        server = _hello_server(
            address,
            ipc_overrides={'heartbeat_interval': 0.3, 'heartbeat_timeout': 0.8},
        )
        server.start()
        try:
            _wait_for_server(address)
            proxy = cc.connect(Hello, name='hello', address=address)
            try:
                # Make repeated calls over 1s (longer than heartbeat_interval)
                for _ in range(4):
                    result = proxy.greeting('HB')
                    assert result == 'Hello, HB!'
                    time.sleep(0.25)
                # Client should still be functional
                result = proxy.add(1, 2)
                assert result == 3
            finally:
                cc.close(proxy)
        finally:
            server.shutdown()

    def test_idle_client_survives_heartbeat_with_pong(self):
        """An idle client that responds to PING survives heartbeat probes."""
        address = f'ipc://{_unique_region()}'
        server = _hello_server(
            address,
            ipc_overrides={'heartbeat_interval': 0.2, 'heartbeat_timeout': 0.6},
        )
        server.start()
        try:
            _wait_for_server(address)
            proxy = cc.connect(Hello, name='hello', address=address)
            try:
                # Do one call then go idle for longer than heartbeat_interval
                result = proxy.greeting('Idle')
                assert result == 'Hello, Idle!'
                # Idle — client auto-responds to PING
                time.sleep(0.5)
                # Should still work
                result = proxy.add(10, 20)
                assert result == 30
            finally:
                cc.close(proxy)
        finally:
            server.shutdown()

    def test_dead_client_detected_by_heartbeat(self):
        """A raw socket (no PONG capability) is detected within timeout.

        This tests low-level transport behavior — the server must detect
        and clean up dead connections without crashing.
        """
        address = f'ipc://{_unique_region()}'
        server = _hello_server(
            address,
            ipc_overrides={'heartbeat_interval': 0.1, 'heartbeat_timeout': 0.3},
        )
        server.start()
        try:
            _wait_for_server(address)
            # A separate process bounds blocking named-pipe reads on Windows.
            result = subprocess.run(
                [sys.executable, '-c', r"""
import errno
import socket
import sys

if sys.platform == 'win32':
    connection = open(sys.argv[1], 'r+b', buffering=0)
    read = connection.read
else:
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    connection.settimeout(2.0)
    connection.connect(sys.argv[1])
    read = connection.recv
received = bytearray()
try:
    while True:
        try:
            chunk = read(4096)
        except TimeoutError as error:
            raise AssertionError(f"connection stayed open after {len(received)} bytes: {received.hex()}") from error
        except OSError as error:
            if error.errno in (errno.EPIPE, errno.ECONNRESET) or getattr(error, 'winerror', None) in (109, 233):
                break
            raise
        if not chunk:
            break
        received.extend(chunk)
finally:
    connection.close()
# The server sent a PING before retiring the non-responsive connection.
assert len(received) >= 17, received
assert received[16] == 1, received
""", _endpoint_name_from_address(address)],
                capture_output=True,
                text=True,
                timeout=5.0,
            )
            assert result.returncode == 0, result.stdout + result.stderr

            # Server should still be functional for new clients
            assert ping(address, timeout=1.0)
        finally:
            server.shutdown()

    def test_heartbeat_disabled(self):
        """When heartbeat_interval=0, no probes are sent."""
        address = f'ipc://{_unique_region()}'
        server = _hello_server(
            address,
            ipc_overrides={'heartbeat_interval': 0, 'heartbeat_timeout': 30},
        )
        server.start()
        try:
            _wait_for_server(address)
            proxy = cc.connect(Hello, name='hello', address=address)
            try:
                # Idle for a while — should NOT be disconnected
                time.sleep(0.3)
                result = proxy.greeting('OK')
                assert result == 'Hello, OK!'
            finally:
                cc.close(proxy)
        finally:
            server.shutdown()
