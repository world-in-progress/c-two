"""Unit tests for ``cc.serve()`` — CRM resource daemon mode."""
from __future__ import annotations

import logging
import threading
import time
from unittest.mock import patch

import pytest

import c_two as cc
from c_two.transport.registry import _ProcessRegistry


# -- Helpers ---------------------------------------------------------------

@cc.crm(namespace='cc.test.serve', version='0.1.0')
class IServeCRM:
    def ping(self) -> str:
        ...


class ServeCRM:
    def ping(self) -> str:
        return 'pong'


# -- Tests -----------------------------------------------------------------

class TestServeBasic:
    """Basic cc.serve() behavior tests."""

    def setup_method(self):
        self.registry = _ProcessRegistry()

    def teardown_method(self):
        self.registry._serve_stop = None
        try:
            self.registry.shutdown()
        except Exception:
            pass

    def test_serve_no_crm_warns(self, caplog):
        """serve() with no CRM registered logs a warning but doesn't crash."""
        with caplog.at_level(logging.WARNING):
            self.registry.serve(blocking=False)

        assert any('no crm registered' in r.message.lower() for r in caplog.records)

    @patch.object(_ProcessRegistry, '_relay_register')
    def test_serve_non_blocking_returns(self, mock_relay):
        """serve(blocking=False) returns immediately."""
        self.registry.register(IServeCRM, ServeCRM(), name='svc')

        start = time.monotonic()
        self.registry.serve(blocking=False)
        elapsed = time.monotonic() - start

        assert elapsed < 1.0  # Should return nearly instantly.

    @patch.object(_ProcessRegistry, '_relay_register')
    def test_serve_idempotent(self, mock_relay):
        """Calling serve() twice is a no-op (idempotent)."""
        self.registry.register(IServeCRM, ServeCRM(), name='svc')

        self.registry.serve(blocking=False)
        self.registry.serve(blocking=False)  # Should not raise.

    @patch.object(_ProcessRegistry, '_relay_register')
    def test_serve_blocking_unblocks_on_stop(self, mock_relay):
        """serve(blocking=True) blocks until the stop event is set."""
        self.registry.register(IServeCRM, ServeCRM(), name='svc')

        done = threading.Event()

        def _run():
            self.registry.serve(blocking=True)
            done.set()

        t = threading.Thread(target=_run, daemon=True)
        t.start()

        # Give it a moment to start blocking.
        time.sleep(0.1)
        assert not done.is_set()

        # Trigger stop.
        self.registry._serve_stop.set()
        done.wait(timeout=5.0)
        assert done.is_set()

    @patch.object(_ProcessRegistry, '_relay_register')
    def test_serve_prints_banner(self, mock_relay, capsys):
        """serve() prints a banner with CRM route info."""
        self.registry.register(IServeCRM, ServeCRM(), name='my_service')
        self.registry.serve(blocking=False)

        captured = capsys.readouterr()
        assert 'C-Two Resource Server' in captured.out
        assert 'my_service' in captured.out
        assert 'Serving 1 CRM resource(s)' in captured.out

    def test_serve_no_crm_prints_empty_banner(self, capsys):
        """serve() with no CRM shows (no CRM routes registered)."""
        self.registry.serve(blocking=False)

        captured = capsys.readouterr()
        assert 'no CRM routes registered' in captured.out
        assert 'Serving 0 CRM resource(s)' in captured.out


class TestServeExport:
    """Verify cc.serve is accessible at the top-level namespace."""

    def test_serve_exists(self):
        assert hasattr(cc, 'serve')
        assert callable(cc.serve)
