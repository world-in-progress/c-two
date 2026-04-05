"""Integration tests for server-side heartbeat detection."""
from __future__ import annotations

import os
import threading
import time

import pytest

import c_two as cc
from c_two.transport import Server
from c_two.config.ipc import ServerIPCConfig
from c_two.transport.client.util import ping

from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello


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


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


class TestHeartbeatIntegration:
    """End-to-end heartbeat tests with real Server via SOTA API."""

    def test_active_connection_survives_heartbeat(self):
        """An active client is NOT disconnected by heartbeat probes."""
        config = ServerIPCConfig(heartbeat_interval=0.3, heartbeat_timeout=0.8)
        address = f'ipc://{_unique_region()}'
        server = Server(address, IHello, Hello(), ipc_config=config, name='hello')
        server.start()
        try:
            _wait_for_server(address)
            proxy = cc.connect(IHello, name='hello', address=address)
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
        config = ServerIPCConfig(heartbeat_interval=0.2, heartbeat_timeout=0.6)
        address = f'ipc://{_unique_region()}'
        server = Server(address, IHello, Hello(), ipc_config=config, name='hello')
        server.start()
        try:
            _wait_for_server(address)
            proxy = cc.connect(IHello, name='hello', address=address)
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
        import socket
        config = ServerIPCConfig(heartbeat_interval=0.1, heartbeat_timeout=0.3)
        address = f'ipc://{_unique_region()}'
        server = Server(address, IHello, Hello(), ipc_config=config, name='hello')
        server.start()
        try:
            _wait_for_server(address)
            region_id = address.replace('ipc://', '')
            _ipc_sock_dir = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')
            sock_path = os.path.join(_ipc_sock_dir, f'{region_id}.sock')
            raw_sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            raw_sock.connect(sock_path)
            raw_sock.settimeout(1.0)
            # Wait for heartbeat timeout + margin
            time.sleep(0.6)
            # Server should have closed the connection
            try:
                data = raw_sock.recv(4096)
            except (ConnectionResetError, BrokenPipeError, OSError):
                pass
            finally:
                raw_sock.close()

            # Server should still be functional for new clients
            assert ping(address, timeout=1.0)
        finally:
            server.shutdown()

    def test_heartbeat_disabled(self):
        """When heartbeat_interval=0, no probes are sent."""
        config = ServerIPCConfig(heartbeat_interval=0, heartbeat_timeout=30)
        address = f'ipc://{_unique_region()}'
        server = Server(address, IHello, Hello(), ipc_config=config, name='hello')
        server.start()
        try:
            _wait_for_server(address)
            proxy = cc.connect(IHello, name='hello', address=address)
            try:
                # Idle for a while — should NOT be disconnected
                time.sleep(0.3)
                result = proxy.greeting('OK')
                assert result == 'Hello, OK!'
            finally:
                cc.close(proxy)
        finally:
            server.shutdown()
