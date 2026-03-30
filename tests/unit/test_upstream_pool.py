"""Unit tests for UpstreamPool — idle eviction, dead detection, lazy reconnect."""
from __future__ import annotations

import threading
import time
from unittest.mock import MagicMock, patch

import pytest

from c_two.transport.relay.core import UpstreamPool, _UpstreamEntry


def _make_pool(idle_timeout: float = 300.0) -> UpstreamPool:
    """Create an UpstreamPool without triggering __init__ (no real IPC)."""
    pool = UpstreamPool.__new__(UpstreamPool)
    pool._entries = {}
    pool._lock = threading.Lock()
    pool._ipc_config = None
    pool._idle_timeout = idle_timeout
    pool._sweeper_timer = None
    return pool


def _make_entry(name: str, address: str = 'ipc-v3://test',
                client: MagicMock | None = None,
                age: float = 0.0) -> _UpstreamEntry:
    """Create an _UpstreamEntry with controllable age."""
    entry = _UpstreamEntry.__new__(_UpstreamEntry)
    entry.name = name
    entry.address = address
    entry.client = client if client is not None else MagicMock()
    entry.last_activity = time.monotonic() - age
    return entry


def _make_client(closed: bool = False) -> MagicMock:
    mock = MagicMock()
    mock._closed = closed
    return mock


# -- Basic lifecycle -------------------------------------------------------

class TestUpstreamPoolBasicLifecycle:
    """Register, get, remove, has (basic CRUD via manual _entries)."""

    def test_add_and_get(self):
        pool = _make_pool()
        client = _make_client()
        pool._entries['r1'] = _make_entry('r1', client=client)

        assert pool.get('r1') is client

    def test_get_unknown_returns_none(self):
        pool = _make_pool()
        assert pool.get('nonexistent') is None

    def test_has_returns_true_for_registered(self):
        pool = _make_pool()
        pool._entries['r1'] = _make_entry('r1')
        assert pool.has('r1') is True

    def test_has_returns_true_for_evicted(self):
        pool = _make_pool()
        entry = _make_entry('r1')
        entry.client = None  # evicted
        pool._entries['r1'] = entry
        assert pool.has('r1') is True

    def test_has_returns_false_for_unknown(self):
        pool = _make_pool()
        assert pool.has('unknown') is False

    def test_remove_pops_entry(self):
        pool = _make_pool()
        client = _make_client()
        pool._entries['r1'] = _make_entry('r1', client=client)
        pool.remove('r1')

        assert pool.has('r1') is False
        client.terminate.assert_called_once()

    def test_remove_unknown_raises_key_error(self):
        pool = _make_pool()
        with pytest.raises(KeyError):
            pool.remove('ghost')


# -- Touch ----------------------------------------------------------------

class TestUpstreamPoolTouch:
    """touch() updates last_activity timestamp."""

    def test_touch_updates_timestamp(self):
        pool = _make_pool()
        entry = _make_entry('r1', age=100.0)
        pool._entries['r1'] = entry
        old = entry.last_activity

        time.sleep(0.01)
        pool.touch('r1')
        assert entry.last_activity > old

    def test_touch_nonexistent_is_noop(self):
        pool = _make_pool()
        pool.touch('ghost')  # should not raise


# -- Sweep idle -----------------------------------------------------------

class TestUpstreamPoolSweepIdle:
    """Sweeper evicts idle and dead connections."""

    def test_sweep_evicts_idle_entry(self):
        pool = _make_pool(idle_timeout=0.01)
        client = _make_client()
        pool._entries['r1'] = _make_entry('r1', client=client, age=1.0)

        pool._sweep_idle()

        assert pool._entries['r1'].client is None
        client.terminate.assert_called_once()

    def test_sweep_evicts_dead_entry(self):
        pool = _make_pool(idle_timeout=300.0)
        client = _make_client(closed=True)
        # Just touched — still evicted because _closed is True
        pool._entries['r1'] = _make_entry('r1', client=client, age=0.0)

        pool._sweep_idle()

        assert pool._entries['r1'].client is None
        client.terminate.assert_called_once()

    def test_sweep_dead_even_when_timeout_zero(self):
        pool = _make_pool(idle_timeout=0)
        client = _make_client(closed=True)
        pool._entries['r1'] = _make_entry('r1', client=client, age=0.0)

        pool._sweep_idle()

        assert pool._entries['r1'].client is None
        client.terminate.assert_called_once()

    def test_sweep_no_time_eviction_when_timeout_zero(self):
        """Alive + old entry should NOT be evicted when idle_timeout=0."""
        pool = _make_pool(idle_timeout=0)
        client = _make_client(closed=False)
        pool._entries['r1'] = _make_entry('r1', client=client, age=9999.0)

        pool._sweep_idle()

        assert pool._entries['r1'].client is client
        client.terminate.assert_not_called()

    def test_sweep_skips_already_evicted(self):
        """Entry with client=None should be harmlessly skipped."""
        pool = _make_pool(idle_timeout=0.01)
        entry = _make_entry('r1', age=100.0)
        entry.client = None
        pool._entries['r1'] = entry

        pool._sweep_idle()  # no crash, no terminate call

    def test_touch_prevents_idle_eviction(self):
        pool = _make_pool(idle_timeout=300.0)
        client = _make_client()
        pool._entries['r1'] = _make_entry('r1', client=client, age=0.0)

        pool.touch('r1')
        pool._sweep_idle()

        assert pool._entries['r1'].client is client
        client.terminate.assert_not_called()

    def test_sweep_retains_address_after_eviction(self):
        pool = _make_pool(idle_timeout=0.01)
        client = _make_client()
        pool._entries['r1'] = _make_entry('r1', address='ipc-v3://addr1',
                                          client=client, age=1.0)

        pool._sweep_idle()

        entry = pool._entries['r1']
        assert entry.client is None
        assert entry.address == 'ipc-v3://addr1'
        assert entry.name == 'r1'


# -- Lazy reconnect -------------------------------------------------------

class TestUpstreamPoolLazyReconnect:
    """get() triggers reconnect for evicted entries."""

    @patch('c_two.transport.relay.core.SharedClient')
    def test_get_reconnects_evicted_entry(self, MockSharedClient):
        pool = _make_pool()
        entry = _make_entry('r1', address='ipc-v3://addr1')
        entry.client = None  # evicted
        pool._entries['r1'] = entry

        new_client = MagicMock()
        MockSharedClient.return_value = new_client

        result = pool.get('r1')

        MockSharedClient.assert_called_once_with('ipc-v3://addr1', None)
        new_client.connect.assert_called_once()
        assert result is new_client
        assert entry.client is new_client

    @patch('c_two.transport.relay.core.SharedClient')
    def test_get_reconnect_failure_returns_none(self, MockSharedClient):
        pool = _make_pool()
        entry = _make_entry('r1', address='ipc-v3://addr1')
        entry.client = None
        pool._entries['r1'] = entry

        MockSharedClient.return_value.connect.side_effect = ConnectionError('refused')

        result = pool.get('r1')

        assert result is None
        assert entry.client is None


# -- Shutdown -------------------------------------------------------------

class TestUpstreamPoolShutdown:
    """shutdown() cancels sweeper and terminates all clients."""

    def test_shutdown_cancels_sweeper_and_terminates(self):
        pool = _make_pool()
        timer = MagicMock()
        pool._sweeper_timer = timer

        c1 = _make_client()
        c2 = _make_client()
        pool._entries['r1'] = _make_entry('r1', client=c1)
        pool._entries['r2'] = _make_entry('r2', client=c2)

        pool.shutdown()

        timer.cancel.assert_called_once()
        assert pool._sweeper_timer is None
        c1.terminate.assert_called_once()
        c2.terminate.assert_called_once()
        assert len(pool._entries) == 0

    def test_shutdown_skips_none_clients(self):
        pool = _make_pool()
        pool._sweeper_timer = None

        entry = _make_entry('r1')
        entry.client = None
        pool._entries['r1'] = entry

        pool.shutdown()  # no crash
        assert len(pool._entries) == 0
