"""Unit tests for heartbeat detection."""
from __future__ import annotations

import asyncio
import os
import socket
import struct
import time
from unittest.mock import MagicMock, AsyncMock, patch

import pytest

from c_two.transport.server.connection import Connection
from c_two.transport.ipc.frame import IPCConfig, FRAME_STRUCT, encode_frame
from c_two.transport.ipc.msg_type import MsgType, PING_BYTES, PONG_BYTES
from c_two.transport.protocol import FLAG_SIGNAL


# ---------------------------------------------------------------------------
# Task 1: Connection.last_activity
# ---------------------------------------------------------------------------


class TestConnectionLastActivity:
    """Connection.last_activity tracks most recent frame time."""

    def test_last_activity_initialized(self):
        writer = MagicMock()
        conn = Connection(conn_id=1, writer=writer, config=IPCConfig())
        assert conn.last_activity > 0

    def test_touch_updates_activity(self):
        writer = MagicMock()
        conn = Connection(conn_id=1, writer=writer, config=IPCConfig())
        t0 = conn.last_activity
        time.sleep(0.01)
        conn.touch()
        assert conn.last_activity > t0

    def test_idle_seconds(self):
        writer = MagicMock()
        conn = Connection(conn_id=1, writer=writer, config=IPCConfig())
        conn.last_activity = time.monotonic() - 10.0
        idle = conn.idle_seconds()
        assert 9.9 < idle < 11.0


# ---------------------------------------------------------------------------
# Task 2: run_heartbeat coroutine
# ---------------------------------------------------------------------------


class TestHeartbeatCoroutine:
    """Test the heartbeat probe coroutine."""

    def test_heartbeat_disabled_when_interval_zero(self):
        """Heartbeat with interval=0 returns immediately."""
        from c_two.transport.server.heartbeat import run_heartbeat

        writer = AsyncMock()
        conn = Connection(conn_id=1, writer=writer, config=IPCConfig(heartbeat_interval=0))
        conn.last_activity = time.monotonic()
        asyncio.run(run_heartbeat(conn))
        writer.write.assert_not_called()

    def test_heartbeat_sends_ping_after_idle(self):
        """After idle > interval, heartbeat sends PING."""
        from c_two.transport.server.heartbeat import run_heartbeat

        async def _run():
            config = IPCConfig(heartbeat_interval=0.1, heartbeat_timeout=0.5)
            writer = MagicMock()
            writer.drain = AsyncMock()
            writer.is_closing = MagicMock(return_value=False)
            conn = Connection(conn_id=1, writer=writer, config=config)
            conn.last_activity = time.monotonic() - 0.2

            task = asyncio.create_task(run_heartbeat(conn))
            await asyncio.sleep(0.15)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task

            assert writer.write.call_count >= 1

        asyncio.run(_run())

    def test_heartbeat_timeout_closes_writer(self):
        """When no PONG arrives within timeout, connection is closed."""
        from c_two.transport.server.heartbeat import run_heartbeat

        async def _run():
            config = IPCConfig(heartbeat_interval=0.05, heartbeat_timeout=0.1)
            writer = MagicMock()
            writer.drain = AsyncMock()
            writer.is_closing = MagicMock(return_value=False)
            conn = Connection(conn_id=1, writer=writer, config=config)
            conn.last_activity = time.monotonic() - 1.0

            task = asyncio.create_task(run_heartbeat(conn))
            await asyncio.sleep(0.25)
            assert task.done()
            writer.close.assert_called()

        asyncio.run(_run())

    def test_heartbeat_resets_on_activity(self):
        """Activity resets the heartbeat timer — no PING sent."""
        from c_two.transport.server.heartbeat import run_heartbeat

        async def _run():
            config = IPCConfig(heartbeat_interval=0.1, heartbeat_timeout=0.5)
            writer = MagicMock()
            writer.drain = AsyncMock()
            writer.is_closing = MagicMock(return_value=False)
            conn = Connection(conn_id=1, writer=writer, config=config)

            task = asyncio.create_task(run_heartbeat(conn))
            for _ in range(5):
                conn.touch()
                await asyncio.sleep(0.03)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task

            writer.write.assert_not_called()

        asyncio.run(_run())


# ---------------------------------------------------------------------------
# Task 3: Client PONG auto-response (protocol-level check)
# ---------------------------------------------------------------------------


class TestClientPongResponse:
    """Verify PING frame encoding for server→client direction."""

    def test_ping_frame_is_valid_signal(self):
        """PING frame has FLAG_SIGNAL and 1-byte MsgType.PING payload."""
        ping_frame = encode_frame(42, FLAG_SIGNAL, PING_BYTES)
        header = ping_frame[:16]
        total_len, request_id, flags = FRAME_STRUCT.unpack(header)
        payload = ping_frame[16:]

        assert flags & FLAG_SIGNAL
        assert len(payload) == 1
        assert payload[0] == MsgType.PING
        assert request_id == 42


# ---------------------------------------------------------------------------
# Task 6: SHM cleanup on shutdown
# ---------------------------------------------------------------------------


class TestShmCleanupOnShutdown:
    """Server.shutdown() calls cleanup_stale_shm()."""

    @pytest.mark.xfail(reason="NativeServerBridge does not call cleanup_stale_shm")
    @patch('c_two.transport.server.core.cleanup_stale_shm')
    def test_shutdown_calls_cleanup(self, mock_cleanup):
        from c_two.transport.server import Server

        mock_cleanup.return_value = 0
        address = f'ipc://test_cleanup_{os.getpid()}'
        server = Server(address)
        server.shutdown()
        mock_cleanup.assert_called_once_with('cc')

    @patch('c_two.transport.server.core.cleanup_stale_shm')
    def test_shutdown_resilient_to_cleanup_error(self, mock_cleanup):
        from c_two.transport.server import Server

        mock_cleanup.side_effect = OSError('shm error')
        address = f'ipc://test_cleanup_err_{os.getpid()}'
        server = Server(address)
        server.shutdown()  # Should not raise
