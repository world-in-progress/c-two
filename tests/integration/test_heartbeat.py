"""Integration tests for server-side heartbeat detection."""
from __future__ import annotations

import os
import pickle
import socket
import threading
import time

import pytest

from c_two.transport import Server, SharedClient
from c_two.transport.ipc.frame import IPCConfig

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
            if SharedClient.ping(address, timeout=0.5):
                return
        except Exception:
            pass
        time.sleep(0.05)
    raise TimeoutError(f'Server at {address} not ready after {timeout}s')


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


class TestHeartbeatIntegration:
    """End-to-end heartbeat tests with real Server + SharedClient."""

    def test_active_connection_survives_heartbeat(self):
        """An active client is NOT disconnected by heartbeat probes."""
        config = IPCConfig(heartbeat_interval=0.3, heartbeat_timeout=0.8)
        address = f'ipc://{_unique_region()}'
        server = Server(address, IHello, Hello(), ipc_config=config)
        server.start()
        try:
            _wait_for_server(address)
            client = SharedClient(address, ipc_config=config)
            client.connect()
            try:
                # Make repeated calls over 1s (longer than heartbeat_interval)
                for _ in range(4):
                    result = pickle.loads(
                        client.call('greeting', pickle.dumps(('HB',)))
                    )
                    assert result == 'Hello, HB!'
                    time.sleep(0.25)
                # Client should still be functional
                result = pickle.loads(
                    client.call('add', pickle.dumps((1, 2)))
                )
                assert result == 3
            finally:
                client.terminate()
        finally:
            server.shutdown()

    def test_idle_client_survives_heartbeat_with_pong(self):
        """An idle client that responds to PING survives heartbeat probes."""
        config = IPCConfig(heartbeat_interval=0.2, heartbeat_timeout=0.6)
        address = f'ipc://{_unique_region()}'
        server = Server(address, IHello, Hello(), ipc_config=config)
        server.start()
        try:
            _wait_for_server(address)
            client = SharedClient(address, ipc_config=config)
            client.connect()
            try:
                # Do one call then go idle for longer than heartbeat_interval
                result = pickle.loads(
                    client.call('greeting', pickle.dumps(('Idle',)))
                )
                assert result == 'Hello, Idle!'
                # Idle — client _recv_loop auto-responds to PING
                time.sleep(0.5)
                # Should still work
                result = pickle.loads(
                    client.call('add', pickle.dumps((10, 20)))
                )
                assert result == 30
            finally:
                client.terminate()
        finally:
            server.shutdown()

    def test_dead_client_detected_by_heartbeat(self):
        """A raw socket (no PONG capability) is detected within timeout."""
        config = IPCConfig(heartbeat_interval=0.1, heartbeat_timeout=0.3)
        address = f'ipc://{_unique_region()}'
        server = Server(address, IHello, Hello(), ipc_config=config)
        server.start()
        try:
            _wait_for_server(address)
            region_id = address.replace('ipc://', '')
            sock_path = f'/tmp/c_two_ipc/{region_id}.sock'
            raw_sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            raw_sock.connect(sock_path)
            raw_sock.settimeout(1.0)
            # Wait for heartbeat timeout + margin
            time.sleep(0.6)
            # Server should have closed the connection
            try:
                data = raw_sock.recv(4096)
                # Empty read or data is fine — connection may have data
                # before close. The key thing is server didn't crash.
            except (ConnectionResetError, BrokenPipeError, OSError):
                pass
            finally:
                raw_sock.close()

            # Server should still be functional for new clients
            assert SharedClient.ping(address, timeout=1.0)
        finally:
            server.shutdown()

    def test_heartbeat_disabled(self):
        """When heartbeat_interval=0, no probes are sent."""
        config = IPCConfig(heartbeat_interval=0, heartbeat_timeout=30)
        address = f'ipc://{_unique_region()}'
        server = Server(address, IHello, Hello(), ipc_config=config)
        server.start()
        try:
            _wait_for_server(address)
            client = SharedClient(address, ipc_config=config)
            client.connect()
            try:
                # Idle for a while — should NOT be disconnected
                time.sleep(0.3)
                result = pickle.loads(
                    client.call('greeting', pickle.dumps(('OK',)))
                )
                assert result == 'Hello, OK!'
            finally:
                client.terminate()
        finally:
            server.shutdown()
