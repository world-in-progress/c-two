# v0.3.0 Pre-Release Review & Hardening Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden the transport layer with edge-case tests, fix functional gaps found during review, and document architecture improvements for post-release.

**Architecture:** Three phases — (1) Write unit tests for all recent features before touching production code, (2) Fix actual bugs and forgotten functionality, (3) Record architecture findings. All changes are additive or surgical — no large-scale refactors before release.

**Tech Stack:** Python 3.10+, pytest, unittest.mock, Rust (cargo test), Click (CLI testing)

---

## Findings Summary

### Functional Gaps (Bugs)

| ID | Issue | Severity | Location |
|----|-------|----------|----------|
| BUG-1 | `UpstreamPool.get()` returns dead client (`_closed=True`) without checking, causing guaranteed 502 on first request after CRM death | 🔴 HIGH | `relay/core.py:142-154` |
| BUG-2 | Rust `pool.get()` returns disconnected client (`!is_connected()`) — same issue | 🔴 HIGH | `c2-relay/src/state.rs:79-83` |
| BUG-3 | Rust relay `call_handler` doesn't evict on call failure — dead client stays until sweeper | 🟠 MED | `c2-relay/src/router.rs:221-227` |

### Forgotten Functionality

| ID | Issue | Severity | Location |
|----|-------|----------|----------|
| FF-1 | Rust IpcClient recv_loop ignores `DISCONNECT_ACK` — `SIG_DISCONNECT_ACK` defined but never matched | 🟡 LOW | `c2-ipc/src/client.rs:398-411` |
| FF-2 | `MsgType.SHUTDOWN_SERVER` (0x06) defined but never used in any handler | 🟡 LOW | `ipc/msg_type.py:18` |

### Architecture (Deferred to post-0.3.0)

| Issue | File | Lines |
|-------|------|-------|
| SharedClient monolith | `client/core.py` | 1017 |
| Server monolith | `server/core.py` | 911 |
| Registry dual-concern | `registry.py` | 731 |
| wire.py mixes codec + MethodTable | `wire.py` | 505 |

---

## Phase 1: Edge Case Tests

### Task 1: UpstreamPool Unit Tests

Write comprehensive unit tests for `UpstreamPool` covering idle eviction, dead connection detection, lazy reconnect, and edge cases.

**Files:**
- Create: `tests/unit/test_upstream_pool.py`

- [ ] **Step 1: Write the test file skeleton and basic lifecycle tests**

```python
"""Unit tests for UpstreamPool idle tracking, sweeper, and lazy reconnect."""
from __future__ import annotations

import threading
import time
from unittest.mock import MagicMock, patch

import pytest

from c_two.transport.relay.core import UpstreamPool, _UpstreamEntry


def _make_mock_client(*, closed: bool = False) -> MagicMock:
    """Create a mock SharedClient with controllable _closed flag."""
    client = MagicMock()
    client._closed = closed
    client.terminate = MagicMock()
    client.connect = MagicMock()
    return client


class TestUpstreamPoolBasicLifecycle:
    """Register, get, remove, has — basic CRUD."""

    def test_add_and_get(self):
        pool = UpstreamPool.__new__(UpstreamPool)
        pool._entries = {}
        pool._lock = threading.Lock()
        pool._ipc_config = None
        pool._idle_timeout = 0
        pool._sweeper_timer = None
        # Manually insert to avoid real IPC connect
        client = _make_mock_client()
        pool._entries['grid'] = _UpstreamEntry('grid', 'ipc://g', client)
        assert pool.get('grid') is client

    def test_get_unknown_returns_none(self):
        pool = UpstreamPool.__new__(UpstreamPool)
        pool._entries = {}
        pool._lock = threading.Lock()
        assert pool.get('unknown') is None

    def test_has_returns_true_for_registered(self):
        pool = UpstreamPool.__new__(UpstreamPool)
        pool._entries = {}
        pool._lock = threading.Lock()
        pool._entries['grid'] = _UpstreamEntry('grid', 'ipc://g', _make_mock_client())
        assert pool.has('grid')

    def test_has_returns_true_for_evicted_entry(self):
        pool = UpstreamPool.__new__(UpstreamPool)
        pool._entries = {}
        pool._lock = threading.Lock()
        pool._entries['grid'] = _UpstreamEntry('grid', 'ipc://g', None)
        assert pool.has('grid')

    def test_has_returns_false_for_unknown(self):
        pool = UpstreamPool.__new__(UpstreamPool)
        pool._entries = {}
        pool._lock = threading.Lock()
        assert not pool.has('unknown')


class TestUpstreamPoolTouch:
    """touch() updates last_activity."""

    def test_touch_updates_timestamp(self):
        pool = UpstreamPool.__new__(UpstreamPool)
        pool._entries = {}
        pool._lock = threading.Lock()
        entry = _UpstreamEntry('grid', 'ipc://g', _make_mock_client())
        entry.last_activity = 0.0  # ancient
        pool._entries['grid'] = entry
        pool.touch('grid')
        assert entry.last_activity > 0.0

    def test_touch_nonexistent_is_noop(self):
        pool = UpstreamPool.__new__(UpstreamPool)
        pool._entries = {}
        pool._lock = threading.Lock()
        pool.touch('unknown')  # should not raise


class TestUpstreamPoolSweepIdle:
    """Sweeper evicts idle and dead connections."""

    def _make_pool(self, idle_timeout: float = 1.0) -> UpstreamPool:
        pool = UpstreamPool.__new__(UpstreamPool)
        pool._entries = {}
        pool._lock = threading.Lock()
        pool._ipc_config = None
        pool._idle_timeout = idle_timeout
        pool._sweeper_timer = MagicMock()  # prevent real scheduling
        return pool

    def test_sweep_evicts_idle_entry(self):
        pool = self._make_pool(idle_timeout=0.1)
        client = _make_mock_client()
        entry = _UpstreamEntry('grid', 'ipc://g', client)
        entry.last_activity = time.monotonic() - 10.0  # long ago
        pool._entries['grid'] = entry

        pool._sweep_idle()

        assert entry.client is None, 'Idle client should be evicted'
        client.terminate.assert_called_once()

    def test_sweep_evicts_dead_entry(self):
        pool = self._make_pool(idle_timeout=300.0)
        client = _make_mock_client(closed=True)
        entry = _UpstreamEntry('grid', 'ipc://g', client)
        entry.last_activity = time.monotonic()  # just now — not idle
        pool._entries['grid'] = entry

        pool._sweep_idle()

        assert entry.client is None, 'Dead client should be evicted'
        client.terminate.assert_called_once()

    def test_sweep_dead_even_when_timeout_zero(self):
        """idle_timeout=0 disables time-based eviction but still evicts dead."""
        pool = self._make_pool(idle_timeout=0)
        client = _make_mock_client(closed=True)
        entry = _UpstreamEntry('grid', 'ipc://g', client)
        entry.last_activity = time.monotonic()
        pool._entries['grid'] = entry

        pool._sweep_idle()

        assert entry.client is None, 'Dead connection should be evicted even with timeout=0'

    def test_sweep_no_time_eviction_when_timeout_zero(self):
        """idle_timeout=0 means no time-based eviction."""
        pool = self._make_pool(idle_timeout=0)
        client = _make_mock_client()  # alive
        entry = _UpstreamEntry('grid', 'ipc://g', client)
        entry.last_activity = time.monotonic() - 99999  # very old
        pool._entries['grid'] = entry

        pool._sweep_idle()

        assert entry.client is client, 'Alive client should NOT be evicted when timeout=0'

    def test_sweep_skips_already_evicted(self):
        pool = self._make_pool(idle_timeout=0.1)
        entry = _UpstreamEntry('grid', 'ipc://g', None)
        entry.last_activity = time.monotonic() - 10.0
        pool._entries['grid'] = entry

        pool._sweep_idle()  # should not raise

        assert entry.client is None

    def test_touch_prevents_idle_eviction(self):
        pool = self._make_pool(idle_timeout=1.0)
        client = _make_mock_client()
        entry = _UpstreamEntry('grid', 'ipc://g', client)
        pool._entries['grid'] = entry
        pool.touch('grid')  # reset timestamp

        pool._sweep_idle()

        assert entry.client is client, 'Recently touched client should not be evicted'

    def test_sweep_retains_address_after_eviction(self):
        pool = self._make_pool(idle_timeout=0.1)
        client = _make_mock_client()
        entry = _UpstreamEntry('grid', 'ipc://g', client)
        entry.last_activity = time.monotonic() - 10.0
        pool._entries['grid'] = entry

        pool._sweep_idle()

        assert entry.address == 'ipc://g', 'Address should be retained for reconnect'
        assert pool.has('grid'), 'Entry should still exist after eviction'


class TestUpstreamPoolLazyReconnect:
    """get() triggers reconnect for evicted entries."""

    def _make_pool(self) -> UpstreamPool:
        pool = UpstreamPool.__new__(UpstreamPool)
        pool._entries = {}
        pool._lock = threading.Lock()
        pool._ipc_config = None
        pool._idle_timeout = 300.0
        pool._sweeper_timer = MagicMock()
        return pool

    @patch('c_two.transport.relay.core.SharedClient')
    def test_get_reconnects_evicted_entry(self, MockClient):
        mock_instance = _make_mock_client()
        MockClient.return_value = mock_instance

        pool = self._make_pool()
        entry = _UpstreamEntry('grid', 'ipc://g', None)  # evicted
        pool._entries['grid'] = entry

        result = pool.get('grid')

        assert result is mock_instance
        mock_instance.connect.assert_called_once()
        assert entry.client is mock_instance

    @patch('c_two.transport.relay.core.SharedClient')
    def test_get_reconnect_failure_returns_none(self, MockClient):
        mock_instance = MagicMock()
        mock_instance.connect.side_effect = ConnectionError('refused')
        MockClient.return_value = mock_instance

        pool = self._make_pool()
        entry = _UpstreamEntry('grid', 'ipc://g', None)
        pool._entries['grid'] = entry

        result = pool.get('grid')

        assert result is None
        assert entry.client is None


class TestUpstreamPoolShutdown:
    """shutdown() cancels sweeper and terminates all clients."""

    def test_shutdown_cancels_sweeper_and_terminates(self):
        pool = UpstreamPool.__new__(UpstreamPool)
        pool._entries = {}
        pool._lock = threading.Lock()
        pool._idle_timeout = 300.0
        timer = MagicMock()
        pool._sweeper_timer = timer

        client = _make_mock_client()
        pool._entries['grid'] = _UpstreamEntry('grid', 'ipc://g', client)

        pool.shutdown()

        timer.cancel.assert_called_once()
        client.terminate.assert_called_once()
        assert len(pool._entries) == 0

    def test_shutdown_skips_none_clients(self):
        pool = UpstreamPool.__new__(UpstreamPool)
        pool._entries = {}
        pool._lock = threading.Lock()
        pool._idle_timeout = 0
        pool._sweeper_timer = None

        pool._entries['grid'] = _UpstreamEntry('grid', 'ipc://g', None)

        pool.shutdown()  # should not raise
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_upstream_pool.py -v`
Expected: All tests pass.

- [ ] **Step 3: Commit**

```bash
git add tests/unit/test_upstream_pool.py
git commit -m "test: add comprehensive UpstreamPool unit tests"
```

---

### Task 2: CLI --idle-timeout Tests

Add tests for the new `--idle-timeout` CLI option to `test_cli_relay.py`.

**Files:**
- Modify: `tests/unit/test_cli_relay.py`

- [ ] **Step 1: Add idle-timeout tests to TestRelayClickCommand**

Append to the existing `TestRelayClickCommand` class:

```python
    def test_idle_timeout_in_help(self):
        runner = CliRunner()
        result = runner.invoke(cli, ['relay', '--help'])
        assert '--idle-timeout' in result.output
        assert 'SECONDS' in result.output

    def test_idle_timeout_default_in_help(self):
        runner = CliRunner()
        result = runner.invoke(cli, ['relay', '--help'])
        assert '300' in result.output
```

- [ ] **Step 2: Run tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_cli_relay.py -v`
Expected: All existing + new tests pass.

- [ ] **Step 3: Commit**

```bash
git add tests/unit/test_cli_relay.py
git commit -m "test: add CLI --idle-timeout option tests"
```

---

### Task 3: Rust Idle Sweeper Unit Tests Verification

Verify the 10 existing Rust unit tests in `state.rs` still pass and cover the dead-connection detection logic we added.

**Files:**
- Review: `src/c_two/_native/c2-relay/src/state.rs` (tests at bottom)

- [ ] **Step 1: Run Rust tests**

Run: `cd src/c_two/_native/c2-relay && cargo test -- --nocapture`
Expected: All 10 tests pass.

- [ ] **Step 2: Add test for dead-connection eviction in idle_entries**

Add to the existing `#[cfg(test)]` module in `state.rs`:

```rust
    #[test]
    fn idle_entries_includes_disconnected_client() {
        let pool = UpstreamPool::new();
        let client = make_test_client(false); // is_connected = false
        pool.insert("dead", "ipc://d", Arc::new(client)).unwrap();
        // Touch just now — should still be returned because disconnected
        pool.touch("dead");
        let idle = pool.idle_entries(999_999_999); // huge timeout
        assert_eq!(idle, vec!["dead".to_string()]);
    }
```

Note: This requires the test helper `make_test_client(connected: bool)`. Check existing test helpers — if `make_test_client` doesn't accept a `connected` param, add one. If mocking IpcClient is impractical (it's an async struct with internal state), verify the is_connected() check through integration tests instead and skip this unit test.

- [ ] **Step 3: Commit if changes made**

```bash
cd src/c_two/_native/c2-relay
cargo test
git add src/c_two/_native/c2-relay/src/state.rs
git commit -m "test: add dead-connection eviction test for Rust UpstreamPool"
```

---

## Phase 2: Bug Fixes

### Task 4: Python `get()` — Check Dead Client Before Returning

Fix `UpstreamPool.get()` to check `_closed` before returning a client, triggering immediate reconnect instead of returning a dead client.

**Files:**
- Modify: `src/c_two/transport/relay/core.py:142-154`
- Modify: `tests/unit/test_upstream_pool.py` (add regression test)

- [ ] **Step 1: Write the failing test first**

Add to `tests/unit/test_upstream_pool.py`:

```python
class TestUpstreamPoolGetDeadClient:
    """get() should detect dead clients and trigger reconnect."""

    def _make_pool(self) -> UpstreamPool:
        pool = UpstreamPool.__new__(UpstreamPool)
        pool._entries = {}
        pool._lock = threading.Lock()
        pool._ipc_config = None
        pool._idle_timeout = 300.0
        pool._sweeper_timer = MagicMock()
        return pool

    @patch('c_two.transport.relay.core.SharedClient')
    def test_get_detects_dead_client_and_reconnects(self, MockClient):
        """get() should not return a _closed client — should reconnect."""
        new_client = _make_mock_client()
        MockClient.return_value = new_client

        pool = self._make_pool()
        dead_client = _make_mock_client(closed=True)
        pool._entries['grid'] = _UpstreamEntry('grid', 'ipc://g', dead_client)

        result = pool.get('grid')

        assert result is new_client, 'Should return freshly reconnected client'
        dead_client.terminate.assert_called_once()
        new_client.connect.assert_called_once()

    def test_get_returns_alive_client_directly(self):
        pool = self._make_pool()
        alive = _make_mock_client()
        pool._entries['grid'] = _UpstreamEntry('grid', 'ipc://g', alive)

        result = pool.get('grid')
        assert result is alive
```

- [ ] **Step 2: Run to verify test fails**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_upstream_pool.py::TestUpstreamPoolGetDeadClient -v`
Expected: `test_get_detects_dead_client_and_reconnects` FAILS (current code returns dead client).

- [ ] **Step 3: Fix `get()` in relay/core.py**

Replace current `get()`:

```python
    def get(self, name: str) -> SharedClient | None:
        """Look up a SharedClient by route name.

        If the client was evicted by the idle sweeper or the connection
        is dead, attempts a lazy reconnect before returning ``None``.
        """
        entry = self._entries.get(name)
        if entry is None:
            return None
        if entry.client is not None:
            if not entry.client._closed:
                return entry.client
            # Client is dead — terminate and trigger reconnect
            try:
                entry.client.terminate()
            except Exception:
                pass
            entry.client = None
        # Client was evicted or dead — try lazy reconnect
        return self._try_reconnect(entry)
```

- [ ] **Step 4: Run tests to verify fix**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_upstream_pool.py -v`
Expected: All pass including the new test.

- [ ] **Step 5: Commit**

```bash
git add src/c_two/transport/relay/core.py tests/unit/test_upstream_pool.py
git commit -m "fix(relay): detect dead client in get() and trigger immediate reconnect"
```

---

### Task 5: Rust `pool.get()` — Check `is_connected()` Before Returning

Same fix for the Rust relay: check `is_connected()` in `pool.get()` before returning the client, so the router gets `None` and can try reconnect immediately.

**Files:**
- Modify: `src/c_two/_native/c2-relay/src/state.rs:79-83`

- [ ] **Step 1: Update `get()` to check connection state**

Replace current `get()`:

```rust
    /// Get a cloned Arc to an upstream's IpcClient.
    ///
    /// Returns `None` if the route is not registered, if the client
    /// was evicted, **or** if the client is disconnected (the caller
    /// should trigger lazy reconnect via [`try_reconnect`]).
    pub fn get(&self, name: &str) -> Option<Arc<IpcClient>> {
        let entry = self.entries.get(name)?;
        let client = entry.client.as_ref()?;
        if client.is_connected() {
            Some(client.clone())
        } else {
            None
        }
    }
```

Note: This makes `get()` return `None` for disconnected clients, which triggers the existing `try_reconnect()` path in `call_handler`. The dead client Arc remains in the entry — the sweeper will clean it up on the next cycle. If we want eager cleanup, also set `entry.client = None`, but that requires `&mut self` which changes the caller from read-lock to write-lock. Deferring cleanup to the sweeper is acceptable — the important thing is that the router doesn't use a dead client.

Actually, since `get()` takes `&self` (immutable), we cannot set `entry.client = None`. The sweeper will handle cleanup. The router will call `try_reconnect()` which takes a write lock.

- [ ] **Step 2: Build and run Rust tests**

Run: `cd src/c_two/_native/c2-relay && cargo test -- --nocapture`
Expected: All pass.

- [ ] **Step 3: Rebuild native extension and run full test suite**

Run:
```bash
uv sync --reinstall-package c-two
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```
Expected: 694+ pass.

- [ ] **Step 4: Commit**

```bash
git add src/c_two/_native/c2-relay/src/state.rs
git commit -m "fix(relay): check is_connected() in Rust pool.get() to avoid dead client"
```

---

### Task 6: Rust Router — Evict on Call Failure

When `client.call()` fails with a transport error (not CRM error), the router should evict the dead client so the next request triggers a fresh reconnect rather than hitting the same dead connection.

**Files:**
- Modify: `src/c_two/_native/c2-relay/src/router.rs:221-227`

- [ ] **Step 1: Add eviction on transport error**

Update the error arm in `call_handler`:

```rust
        Err(e) => {
            // Evict the dead client so next request triggers reconnect.
            {
                let mut pool = state.pool.write().unwrap();
                pool.evict(&route_name);
            }
            (
                StatusCode::BAD_GATEWAY,
                [("content-type", "text/plain")],
                format!("relay error: {e}"),
            )
                .into_response()
        }
```

Note: We don't close the old client here — `evict()` returns `Option<Arc<IpcClient>>` but the Arc may still be referenced by other in-flight requests. The client's internal state is already broken, and the sweeper/Drop will clean up.

- [ ] **Step 2: Build and test**

Run:
```bash
cd src/c_two/_native/c2-relay && cargo check
uv sync --reinstall-package c-two
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```
Expected: Clean build, 694+ tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/c_two/_native/c2-relay/src/router.rs
git commit -m "fix(relay): evict dead client on call failure for faster recovery"
```

---

## Phase 3: Architecture Review Document

### Task 7: Create Architecture Findings Document

Record all findings from this review for the post-0.3.0 roadmap. This is a documentation-only task.

**Files:**
- Create: `docs/architecture-review-v030.md`

- [ ] **Step 1: Write the findings document**

The document should contain:

**Section 1: Large File Audit**
- `client/core.py` (1017 lines) — `SharedClient` handles socket I/O, threading, handshake, chunked reassembly, error handling, concurrent call routing. Recommendation: Extract `_ReplyChunkAssembler` to `client/chunking.py`, extract socket management to `client/connection.py`.
- `server/core.py` (911 lines) — `Server` monolith handling asyncio loop, multi-CRM dispatch, method routing, chunk assembly, connection lifecycle. Already partially split into sub-modules (`dispatcher.py`, `heartbeat.py`, `handshake.py`, `reply.py`, `chunk.py`, `connection.py`). The remaining core is the asyncio event loop and connection handler — reasonable for one file.
- `registry.py` (731 lines) — Dual concern: `_ProcessRegistry` lifecycle management + module-level API delegation. Recommendation: Split into `registry.py` (API surface) + `lifecycle.py` (server/client lifecycle management).
- `wire.py` (505 lines) — Mixes codec functions with `MethodTable` class. Recommendation: Extract `MethodTable` to `wire_methods.py`.

**Section 2: Signal Protocol Consistency**
- `SHUTDOWN_SERVER` (0x06): Defined in `MsgType` but never used by any handler. Candidate for removal or documentation as reserved.
- `DISCONNECT_ACK` in Rust recv_loop: The constant `SIG_DISCONNECT_ACK` is defined but the recv_loop only handles PING signals. DISCONNECT_ACK is silently consumed by the `continue` branch. This is functionally correct (close() proceeds after 100ms regardless) but the explicit match would improve clarity.
- `PONG`: Never explicitly handled by clients — received PONGs just fall through to the signal `continue`. This is correct for the current design (PING is fire-and-forget for keepalive).

**Section 3: Concurrency Patterns**
- Python `SharedClient` is synchronous (threading) while `Server` and `Relay` are async (asyncio). No async client variant exists. For post-0.3.0: consider `AsyncSharedClient` for embedded use cases.
- `_WriterPriorityReadWriteLock` in `scheduler.py` is a custom impl. Python 3.13+ has `threading.RWLock` — evaluate migration when min Python version is bumped.

**Section 4: Test Coverage Gaps (Non-Critical)**
- No stress/race-condition tests for concurrent sweeper + reconnect
- No integration test for full idle-evict-reconnect lifecycle
- These are deferred to post-0.3.0 as the unit tests from this plan cover the core logic.

- [ ] **Step 2: Commit**

```bash
git add docs/architecture-review-v030.md
git commit -m "docs: add v0.3.0 architecture review findings"
```

---

## Execution Notes

- **Test first**: Phase 1 (Tasks 1-3) MUST complete before Phase 2 (Tasks 4-6). Tests provide a safety net for the fixes.
- **Batch writes**: All file writes should be done in single operations per file, not incremental edits.
- **Rust rebuild**: After any Rust changes (Tasks 3, 5, 6), run `uv sync --reinstall-package c-two` before Python tests.
- **Test baseline**: 694 passed, 1 known flaky (`test_16_threads_rapid_fire`).
