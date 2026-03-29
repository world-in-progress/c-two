# Heartbeat Detection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement server-side heartbeat probing, connection idle cleanup, and SHM orphan cleanup so dead clients are detected, connections torn down, and leaked shared memory reclaimed.

**Architecture:** The server spawns a per-connection asyncio heartbeat task that periodically sends PING frames. Clients must respond with PONG within `heartbeat_timeout` seconds. On timeout, the server cancels the `_handle_client` task, drains in-flight work, and destroys the connection's buddy pool. On server shutdown, `cleanup_stale_shm()` removes SHM segments owned by dead processes.

**Tech Stack:** Python 3.10+ asyncio, POSIX UDS, Rust FFI (c2-buddy), pytest

**Key Discovery:** SharedClient's `_recv_loop` does NOT auto-respond to server-initiated PING frames. Phase 1 must add PONG response logic to the client.

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `src/c_two/transport/server/connection.py` | Modify | Add `last_activity` field to `Connection` |
| `src/c_two/transport/server/heartbeat.py` | Create | Heartbeat task coroutine (pure function) |
| `src/c_two/transport/server/core.py` | Modify | Integrate heartbeat task + SHM cleanup on shutdown |
| `src/c_two/transport/client/core.py` | Modify | Add PONG auto-response in `_recv_loop` |
| `tests/unit/test_heartbeat.py` | Create | Unit tests for heartbeat logic |
| `tests/integration/test_heartbeat.py` | Create | Integration tests for full heartbeat cycle |

---

## Phase 1: Server Heartbeat Probe + Client PONG Response

### Task 1: Add `last_activity` to Connection

**Files:**
- Modify: `src/c_two/transport/server/connection.py:50-69`

- [ ] **Step 1: Write the failing test**

Create `tests/unit/test_heartbeat.py`:

```python
"""Unit tests for heartbeat detection."""
from __future__ import annotations

import time
from unittest.mock import MagicMock, AsyncMock

import pytest

from c_two.transport.server.connection import Connection
from c_two.transport.ipc.frame import IPCConfig


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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_heartbeat.py::TestConnectionLastActivity -v`
Expected: FAIL — `Connection` has no `last_activity`, `touch()`, or `idle_seconds()`.

- [ ] **Step 3: Add `last_activity`, `touch()`, `idle_seconds()` to Connection**

In `src/c_two/transport/server/connection.py`, add to the `Connection` dataclass:

```python
import time
# ... existing imports ...

@dataclass
class Connection:
    """Per-client connection state."""

    conn_id: int
    writer: asyncio.StreamWriter
    config: IPCConfig
    buddy_pool: object = None
    seg_views: list[memoryview] = field(default_factory=list)
    remote_segment_names: list[str] = field(default_factory=list)
    remote_segment_sizes: list[int] = field(default_factory=list)
    handshake_done: bool = False
    chunked_capable: bool = False
    last_activity: float = field(default_factory=time.monotonic)
    _inflight: int = field(default=0, repr=False)
    _idle: asyncio.Event | None = field(default=None, repr=False)

    def touch(self) -> None:
        """Update last_activity to current time."""
        self.last_activity = time.monotonic()

    def idle_seconds(self) -> float:
        """Seconds since last activity."""
        return time.monotonic() - self.last_activity

    # ... rest of existing methods unchanged ...
```

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_heartbeat.py::TestConnectionLastActivity -v`
Expected: PASS (3/3)

- [ ] **Step 5: Commit**

```bash
git add src/c_two/transport/server/connection.py tests/unit/test_heartbeat.py
git commit -m "feat(heartbeat): add last_activity tracking to Connection"
```

---

### Task 2: Create heartbeat coroutine module

**Files:**
- Create: `src/c_two/transport/server/heartbeat.py`
- Test: `tests/unit/test_heartbeat.py` (append)

- [ ] **Step 1: Write the failing test**

Append to `tests/unit/test_heartbeat.py`:

```python
import asyncio
from c_two.transport.server.heartbeat import run_heartbeat


class TestHeartbeatCoroutine:
    """Test the heartbeat probe coroutine."""

    @pytest.mark.asyncio
    async def test_heartbeat_disabled_when_interval_zero(self):
        """Heartbeat with interval=0 returns immediately."""
        writer = AsyncMock()
        conn = Connection(conn_id=1, writer=writer, config=IPCConfig(heartbeat_interval=0))
        conn.last_activity = time.monotonic()
        # Should return immediately without sending anything
        await run_heartbeat(conn)
        writer.write.assert_not_called()

    @pytest.mark.asyncio
    async def test_heartbeat_sends_ping_after_idle(self):
        """After idle > interval, heartbeat sends PING."""
        config = IPCConfig(heartbeat_interval=0.1, heartbeat_timeout=0.5)
        writer = AsyncMock()
        writer.drain = AsyncMock()
        conn = Connection(conn_id=1, writer=writer, config=config)
        # Pretend we've been idle for longer than heartbeat_interval
        conn.last_activity = time.monotonic() - 0.2

        # Let the heartbeat loop run once then cancel
        task = asyncio.create_task(run_heartbeat(conn))
        await asyncio.sleep(0.15)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task

        # Should have sent at least one PING frame
        assert writer.write.call_count >= 1

    @pytest.mark.asyncio
    async def test_heartbeat_timeout_cancels_writer(self):
        """When no PONG arrives within timeout, connection is closed."""
        config = IPCConfig(heartbeat_interval=0.05, heartbeat_timeout=0.1)
        writer = AsyncMock()
        writer.drain = AsyncMock()
        writer.is_closing = MagicMock(return_value=False)
        conn = Connection(conn_id=1, writer=writer, config=config)
        # Frozen: never gets touched
        conn.last_activity = time.monotonic() - 1.0

        task = asyncio.create_task(run_heartbeat(conn))
        # Wait enough for timeout to fire
        await asyncio.sleep(0.25)
        # Task should have completed (not cancelled) after detecting timeout
        assert task.done()
        # Writer should be closed
        writer.close.assert_called()

    @pytest.mark.asyncio
    async def test_heartbeat_resets_on_activity(self):
        """Activity resets the heartbeat timer — no PING sent."""
        config = IPCConfig(heartbeat_interval=0.1, heartbeat_timeout=0.5)
        writer = AsyncMock()
        writer.drain = AsyncMock()
        conn = Connection(conn_id=1, writer=writer, config=config)

        task = asyncio.create_task(run_heartbeat(conn))
        # Keep touching to simulate active connection
        for _ in range(5):
            conn.touch()
            await asyncio.sleep(0.03)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task

        # No PING sent because connection was always active
        writer.write.assert_not_called()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_heartbeat.py::TestHeartbeatCoroutine -v`
Expected: FAIL — `ImportError: cannot import name 'run_heartbeat'`

- [ ] **Step 3: Implement `run_heartbeat` coroutine**

Create `src/c_two/transport/server/heartbeat.py`:

```python
"""Server-side heartbeat probe.

Spawned as a per-connection asyncio task.  Periodically checks
``conn.idle_seconds()`` and sends a PING frame if the connection
has been idle longer than ``heartbeat_interval``.  If no activity
is observed within ``heartbeat_timeout``, the writer is closed to
signal the ``_handle_client`` frame loop to exit.

Lifecycle:
- Created by ``_handle_client`` after handshake completes.
- Cancelled in ``_handle_client``'s ``finally`` block on disconnect.
- Self-terminates (returns) when timeout triggers writer close.
"""
from __future__ import annotations

import asyncio
import logging

from ..ipc.frame import encode_frame, FLAG_RESPONSE
from ..ipc.msg_type import PING_BYTES
from ..protocol import FLAG_SIGNAL

from .connection import Connection

logger = logging.getLogger(__name__)


async def run_heartbeat(conn: Connection) -> None:
    """Probe client liveness via periodic PING frames.

    Parameters
    ----------
    conn : Connection
        The per-client connection state.  ``conn.config`` supplies
        ``heartbeat_interval`` and ``heartbeat_timeout``.

    The coroutine returns (does not raise) when:
    - ``heartbeat_interval <= 0`` (heartbeat disabled)
    - timeout detected → writer closed
    - writer already closing

    It is expected to be wrapped in ``asyncio.create_task`` and
    cancelled externally on normal disconnect.
    """
    interval = conn.config.heartbeat_interval
    timeout = conn.config.heartbeat_timeout
    if interval <= 0:
        return

    writer = conn.writer
    ping_frame = encode_frame(0, FLAG_SIGNAL, PING_BYTES)

    while True:
        await asyncio.sleep(interval)

        if writer.is_closing():
            return

        idle = conn.idle_seconds()

        if idle >= timeout:
            logger.warning(
                'Conn %d: heartbeat timeout (%.1fs idle, timeout=%.1fs)',
                conn.conn_id, idle, timeout,
            )
            writer.close()
            return

        if idle >= interval:
            writer.write(ping_frame)
            try:
                await writer.drain()
            except (ConnectionResetError, BrokenPipeError, OSError):
                return
```

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_heartbeat.py::TestHeartbeatCoroutine -v`
Expected: PASS (4/4)

- [ ] **Step 5: Commit**

```bash
git add src/c_two/transport/server/heartbeat.py tests/unit/test_heartbeat.py
git commit -m "feat(heartbeat): add run_heartbeat coroutine"
```

---

### Task 3: Client auto-respond to server PING

**Files:**
- Modify: `src/c_two/transport/client/core.py:693-776` (\_recv\_loop)
- Test: `tests/unit/test_heartbeat.py` (append)

**Critical context:** The client `_recv_loop` reads frames in a background thread using blocking socket I/O. When it receives a `FLAG_SIGNAL` frame with `MsgType.PING` payload, it must write back a PONG frame on the same socket. The socket is shared across callers so the write must be protected by `_send_lock`.

- [ ] **Step 1: Write the failing test**

Append to `tests/unit/test_heartbeat.py`:

```python
import socket
import struct

from c_two.transport.ipc.frame import FRAME_STRUCT, encode_frame
from c_two.transport.ipc.msg_type import MsgType, PING_BYTES, PONG_BYTES
from c_two.transport.protocol import FLAG_SIGNAL


class TestClientPongResponse:
    """SharedClient._recv_loop responds to server PING with PONG."""

    def test_client_responds_pong_to_server_ping(self):
        """Simulate server sending PING, verify client sends PONG."""
        # Create a socket pair to simulate server ↔ client
        server_sock, client_sock = socket.socketpair(socket.AF_UNIX)
        try:
            client_sock.settimeout(1.0)
            server_sock.settimeout(1.0)

            # Build a PING frame as server would send
            ping_frame = encode_frame(42, FLAG_SIGNAL, PING_BYTES)

            # Send PING from "server" side
            server_sock.sendall(ping_frame)

            # Read the frame on the "client" side and check detection
            header = client_sock.recv(16)
            total_len, request_id, flags = FRAME_STRUCT.unpack(header)
            payload_len = total_len - 12
            payload = client_sock.recv(payload_len) if payload_len > 0 else b''

            # Verify it's a PING
            assert flags & FLAG_SIGNAL
            assert len(payload) == 1
            assert payload[0] == MsgType.PING
            assert request_id == 42
        finally:
            server_sock.close()
            client_sock.close()
```

Note: This is a protocol-level unit test. The actual integration is tested in Task 6.

- [ ] **Step 2: Run test to verify it passes (protocol verification only)**

Run: `uv run pytest tests/unit/test_heartbeat.py::TestClientPongResponse -v`
Expected: PASS — this validates the frame format, not the client logic.

- [ ] **Step 3: Add PONG auto-response to SharedClient._recv_loop**

In `src/c_two/transport/client/core.py`, modify `_recv_loop` to detect and respond to PING frames. Add this block after the frame header/payload read, **before** the chunked reply handling (before line 718):

```python
# In _recv_loop, after payload is read (after line 715):

                # Auto-respond to server-initiated PING with PONG.
                if flags & FLAG_SIGNAL:
                    if len(payload) == 1 and payload[0] == MsgType.PING:
                        pong = encode_frame(request_id, FLAG_RESPONSE | FLAG_SIGNAL, PONG_BYTES)
                        with self._send_lock:
                            try:
                                sock.sendall(pong)
                            except (ConnectionResetError, BrokenPipeError, OSError):
                                break
                    continue  # Don't dispatch signals to pending callers
```

Also add `PONG_BYTES` to the import from `msg_type` at the top of the file (line 34-39):

```python
from ..ipc.msg_type import (
    MsgType,
    PING_BYTES,
    PONG_BYTES,           # ← ADD
    SHUTDOWN_CLIENT_BYTES,
    SHUTDOWN_ACK_BYTES,
)
```

- [ ] **Step 4: Run full test suite to verify no regression**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: 675+ tests pass, 0 failures

- [ ] **Step 5: Commit**

```bash
git add src/c_two/transport/client/core.py tests/unit/test_heartbeat.py
git commit -m "feat(heartbeat): client auto-responds PONG to server PING"
```

---

### Task 4: Integrate heartbeat into _handle_client

**Files:**
- Modify: `src/c_two/transport/server/core.py:398-587`
- Test: `tests/unit/test_heartbeat.py` (append)

- [ ] **Step 1: Write the failing test**

Append to `tests/unit/test_heartbeat.py`:

```python
class TestServerHeartbeatIntegration:
    """Verify _handle_client touches conn.last_activity on frame receipt."""

    def test_touch_called_on_frame_receipt(self):
        """Verify the integration contract: _handle_client must call
        conn.touch() on every received frame to keep heartbeat alive."""
        # This is a design contract test — actual integration in test_heartbeat_integration.py
        conn = Connection(
            conn_id=1,
            writer=MagicMock(),
            config=IPCConfig(),
        )
        old_ts = conn.last_activity
        time.sleep(0.01)
        conn.touch()
        assert conn.last_activity > old_ts
```

- [ ] **Step 2: Run test to verify it passes (contract test)**

Run: `uv run pytest tests/unit/test_heartbeat.py::TestServerHeartbeatIntegration -v`
Expected: PASS

- [ ] **Step 3: Integrate heartbeat into _handle_client**

In `src/c_two/transport/server/core.py`:

**Add import** (after line 56):
```python
from .heartbeat import run_heartbeat
```

**Add `conn.touch()` in the frame loop** (after line 437, where payload is read):
```python
                conn.touch()
```

**Start heartbeat task after the frame loop begins** (after line 422, before `while True:`):
```python
            heartbeat_task: asyncio.Task | None = None
            if self._config.heartbeat_interval > 0:
                heartbeat_task = asyncio.create_task(run_heartbeat(conn))
```

**Cancel heartbeat in finally block** (add before line 568, the `if pending:` block):
```python
            if heartbeat_task is not None:
                heartbeat_task.cancel()
                try:
                    await heartbeat_task
                except asyncio.CancelledError:
                    pass
```

- [ ] **Step 4: Run full test suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: 675+ tests pass, 0 failures

- [ ] **Step 5: Commit**

```bash
git add src/c_two/transport/server/core.py
git commit -m "feat(heartbeat): integrate heartbeat task into _handle_client"
```

---

## Phase 2: Connection Idle Cleanup

### Task 5: Graceful connection teardown on heartbeat timeout

**Files:**
- Modify: `src/c_two/transport/server/core.py:562-587` (finally block)
- Test: `tests/unit/test_heartbeat.py` (append)

**Context:** When `run_heartbeat` detects timeout, it calls `writer.close()`. This causes `reader.readexactly()` in `_handle_client` to raise `asyncio.IncompleteReadError`, which falls through to the `except` → `finally` block. The `finally` block already handles:
1. Gathering pending asyncio tasks
2. Discarding chunk assemblers
3. Calling `conn.wait_idle()` (drains in-flight thread pool work)
4. Calling `conn.cleanup()` (destroys buddy pool)
5. Closing writer

So the **existing cleanup path is already correct** for heartbeat-triggered disconnects. No new cleanup logic is needed — Phase 2 adds a log message and verifies the behavior with an integration test.

- [ ] **Step 1: Add heartbeat-specific logging to finally block**

In `src/c_two/transport/server/core.py`, modify the `finally` block to log heartbeat timeout vs normal disconnect. After the `except` blocks and before the `finally`:

Add a boolean flag before the `try:` block (after line 414):
```python
        heartbeat_expired = False
```

In the `finally` block, after cancelling heartbeat_task, check if heartbeat triggered the close:
```python
            if heartbeat_task is not None:
                heartbeat_task.cancel()
                try:
                    await heartbeat_task
                except asyncio.CancelledError:
                    pass
                # If heartbeat completed normally (not cancelled), it timed out.
                if heartbeat_task.done() and not heartbeat_task.cancelled():
                    heartbeat_expired = True

            if heartbeat_expired:
                logger.warning(
                    'Conn %d: disconnected (heartbeat timeout)', conn.conn_id,
                )
            else:
                logger.debug('Conn %d: disconnected', conn.conn_id)
```

- [ ] **Step 2: Run full test suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: 675+ tests pass, 0 failures

- [ ] **Step 3: Commit**

```bash
git add src/c_two/transport/server/core.py
git commit -m "feat(heartbeat): add heartbeat timeout logging in cleanup path"
```

---

## Phase 3: SHM PID-Based Cleanup

### Task 6: Server calls cleanup_stale_shm on shutdown

**Files:**
- Modify: `src/c_two/transport/server/core.py:342-390` (shutdown method)
- Test: `tests/unit/test_heartbeat.py` (append)

**Context:** The Rust `cleanup_stale_segments()` scans `/dev/shm` (Linux only) for segments matching a prefix, extracts the PID from the segment name, and unlinks segments whose PID is no longer alive. On macOS it returns 0 (no-op). The Python wrapper is `cleanup_stale_shm(prefix: str) -> int`.

SHM segment naming format: `cc{random_4hex}{pid:08x}_{tag}{counter:04x}` — the prefix is the first 4 chars (e.g. `"cc3b"`). However, each `BuddyPoolHandle` uses a unique 4-char random tag, so the prefix for cleanup should match **all** c-two segments. The common prefix is `"cc"` (2 chars).

- [ ] **Step 1: Write the failing test**

Append to `tests/unit/test_heartbeat.py`:

```python
import sys
from unittest.mock import patch

from c_two.transport.server.core import Server


class TestShmCleanupOnShutdown:
    """Server.shutdown() calls cleanup_stale_shm()."""

    @patch('c_two.transport.server.core.cleanup_stale_shm')
    def test_shutdown_calls_cleanup(self, mock_cleanup):
        """shutdown() invokes cleanup_stale_shm with 'cc' prefix."""
        mock_cleanup.return_value = 0
        address = f'ipc://test_cleanup_{os.getpid()}'
        server = Server(address)
        # Don't actually start — just call shutdown
        server.shutdown()
        mock_cleanup.assert_called_once_with('cc')

    @patch('c_two.transport.server.core.cleanup_stale_shm')
    def test_shutdown_logs_cleaned_segments(self, mock_cleanup):
        """When segments are cleaned, a log message is emitted."""
        mock_cleanup.return_value = 3
        address = f'ipc://test_cleanup_log_{os.getpid()}'
        server = Server(address)
        with patch('c_two.transport.server.core.logger') as mock_logger:
            server.shutdown()
            # Verify info log about cleaned segments
            mock_logger.info.assert_called()

    @patch('c_two.transport.server.core.cleanup_stale_shm')
    def test_shutdown_resilient_to_cleanup_error(self, mock_cleanup):
        """cleanup_stale_shm failure doesn't prevent shutdown."""
        mock_cleanup.side_effect = OSError('shm error')
        address = f'ipc://test_cleanup_err_{os.getpid()}'
        server = Server(address)
        # Should not raise
        server.shutdown()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_heartbeat.py::TestShmCleanupOnShutdown -v`
Expected: FAIL — `cleanup_stale_shm` not imported in core.py, not called in shutdown

- [ ] **Step 3: Add cleanup_stale_shm call to Server.shutdown()**

In `src/c_two/transport/server/core.py`:

**Add import** (after line 37, the buddy imports):
```python
from ...buddy import cleanup_stale_shm
```

**Add SHM cleanup to shutdown()** — at the end of the `shutdown` method, after the socket unlink:

```python
    def shutdown(self) -> None:
        # ... existing code ...
        try:
            os.unlink(self._socket_path)
        except OSError:
            pass

        # Best-effort cleanup of SHM segments owned by dead processes.
        try:
            cleaned = cleanup_stale_shm('cc')
            if cleaned > 0:
                logger.info('Cleaned %d stale SHM segments', cleaned)
        except Exception:
            logger.debug('SHM cleanup failed', exc_info=True)
```

- [ ] **Step 4: Run test to verify it passes**

Run: `uv run pytest tests/unit/test_heartbeat.py::TestShmCleanupOnShutdown -v`
Expected: PASS (3/3)

- [ ] **Step 5: Commit**

```bash
git add src/c_two/transport/server/core.py tests/unit/test_heartbeat.py
git commit -m "feat(heartbeat): call cleanup_stale_shm on server shutdown"
```

---

### Task 7: Integration test — full heartbeat cycle

**Files:**
- Create: `tests/integration/test_heartbeat.py`

This test starts a real Server + SharedClient, configures fast heartbeat timing, verifies that:
1. Normal active connections survive heartbeat probes
2. A dead client (socket closed without graceful shutdown) is detected and cleaned up
3. Server shutdown completes cleanly after heartbeat cleanup

- [ ] **Step 1: Write the integration test**

Create `tests/integration/test_heartbeat.py`:

```python
"""Integration tests for server-side heartbeat detection."""
from __future__ import annotations

import os
import socket
import threading
import time

import pytest

from c_two.transport import Server, SharedClient
from c_two.transport.ipc.frame import IPCConfig
from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello


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


class TestHeartbeatIntegration:
    """End-to-end heartbeat tests with real Server + SharedClient."""

    def test_active_connection_survives_heartbeat(self):
        """An active client is not disconnected by heartbeat probes."""
        config = IPCConfig(heartbeat_interval=0.2, heartbeat_timeout=0.6)
        address = f'ipc://{_unique_region()}'
        server = Server(address, IHello, Hello(), ipc_config=config)
        server.start()
        try:
            _wait_for_server(address)
            client = SharedClient(address, ipc_config=config)
            client.connect()
            try:
                # Make repeated calls over a period longer than heartbeat_interval
                for _ in range(5):
                    # Perform a real RPC call to keep connection alive
                    result = client.call('', 0, b'test')
                    time.sleep(0.15)
                # Connection should still be alive
                assert client.is_connected()
            finally:
                client.terminate()
        finally:
            server.shutdown()

    def test_dead_client_detected_by_heartbeat(self):
        """A client that stops responding is detected within timeout."""
        config = IPCConfig(heartbeat_interval=0.1, heartbeat_timeout=0.3)
        address = f'ipc://{_unique_region()}'
        server = Server(address, IHello, Hello(), ipc_config=config)
        server.start()
        try:
            _wait_for_server(address)
            # Connect a raw socket (no PONG response capability)
            sock_path = f'/tmp/c_two_ipc/{address.replace("ipc://", "")}.sock'
            raw_sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            raw_sock.connect(sock_path)
            # Wait for heartbeat timeout + margin
            time.sleep(0.6)
            # Server should have closed the connection — verify by trying to read
            raw_sock.settimeout(0.5)
            try:
                data = raw_sock.recv(1024)
                # Either empty (closed) or we get a PING/close
                # Connection should be torn down by server
            except (ConnectionResetError, BrokenPipeError, OSError):
                pass  # Expected — server closed the connection
            finally:
                raw_sock.close()
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
                time.sleep(0.5)
                assert client.is_connected()
            finally:
                client.terminate()
        finally:
            server.shutdown()
```

- [ ] **Step 2: Run integration test**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/integration/test_heartbeat.py -v --timeout=30`
Expected: PASS (3/3)

- [ ] **Step 3: Commit**

```bash
git add tests/integration/test_heartbeat.py
git commit -m "test(heartbeat): add integration tests for heartbeat cycle"
```

---

### Task 8: Final verification

- [ ] **Step 1: Run full test suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All tests pass (675+ existing + ~10 new heartbeat tests)

- [ ] **Step 2: Final commit (if any fixups needed)**

```bash
git add -A
git commit -m "chore: heartbeat implementation complete"
```

---

## Summary of Changes

| Phase | What | Where | Lines |
|-------|------|-------|-------|
| 1 | `last_activity` + `touch()` + `idle_seconds()` | `connection.py` | ~15 |
| 1 | `run_heartbeat()` coroutine | `heartbeat.py` (new) | ~55 |
| 1 | Client PONG auto-response | `client/core.py` | ~10 |
| 1 | Heartbeat task integration | `server/core.py` | ~15 |
| 2 | Heartbeat timeout logging | `server/core.py` | ~10 |
| 3 | `cleanup_stale_shm()` on shutdown | `server/core.py` | ~8 |
| Test | Unit tests | `test_heartbeat.py` (new) | ~120 |
| Test | Integration tests | `test_heartbeat.py` (new) | ~100 |
| **Total** | | | **~333** |
