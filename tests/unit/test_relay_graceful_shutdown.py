"""Tests for graceful relay shutdown handling.

Verifies that CRM processes do not crash when the relay is unreachable
during ``cc.unregister()`` or ``cc.shutdown()``.
"""
from __future__ import annotations

import logging
from unittest.mock import patch, MagicMock

import pytest

import c_two as cc
from c_two.rpc_v2.registry import _ProcessRegistry


# -- Helpers ---------------------------------------------------------------

@cc.icrm(namespace='cc.test.relay_shutdown', version='0.1.0')
class IRelayShutdownCRM:
    def ping(self) -> str:
        ...


class RelayShutdownCRM:
    def ping(self) -> str:
        return 'pong'


# -- Tests: unregister tolerates relay absence -----------------------------

class TestUnregisterRelayAbsence:
    """``cc.unregister()`` should not raise when relay is down."""

    def setup_method(self):
        self.registry = _ProcessRegistry()

    def teardown_method(self):
        try:
            self.registry.shutdown()
        except Exception:
            pass

    @patch.object(_ProcessRegistry, '_relay_register')
    @patch.object(_ProcessRegistry, '_relay_unregister')
    def test_unregister_no_relay(self, mock_unreg, mock_reg):
        """Unregister works when relay calls are no-ops."""
        self.registry.register(IRelayShutdownCRM, RelayShutdownCRM(), name='test')
        self.registry.unregister('test')  # Should not raise

    @patch.object(_ProcessRegistry, '_relay_register')
    @patch.object(_ProcessRegistry, '_relay_unregister', side_effect=ConnectionError('relay down'))
    def test_unregister_relay_unreachable(self, mock_unreg, mock_reg, caplog):
        """Unregister logs warning when relay is unreachable (not raise)."""
        self.registry.register(IRelayShutdownCRM, RelayShutdownCRM(), name='test_down')

        # Should NOT raise — just log warning.
        with caplog.at_level(logging.WARNING):
            self.registry.unregister('test_down')

        assert any('unreachable' in r.message.lower() for r in caplog.records)

    @patch.object(_ProcessRegistry, '_relay_register')
    @patch.object(_ProcessRegistry, '_relay_unregister', side_effect=ConnectionError('relay down'))
    def test_shutdown_relay_unreachable(self, mock_unreg, mock_reg, caplog):
        """Shutdown logs info when relay is unreachable (no error)."""
        self.registry.register(IRelayShutdownCRM, RelayShutdownCRM(), name='test_sd')

        with caplog.at_level(logging.INFO):
            self.registry.shutdown()  # Should not raise

        assert any('unreachable' in r.message.lower() or 'relay' in r.message.lower()
                    for r in caplog.records)


# -- Tests: UpstreamPool shutdown logging ----------------------------------

class TestUpstreamPoolShutdownLogging:
    """Relay's UpstreamPool logs upstream names on shutdown."""

    def test_shutdown_logs_upstream_names(self, caplog):
        """UpstreamPool.shutdown() logs names of disconnected upstreams."""
        from c_two.rpc_v2.relay import UpstreamPool, _UpstreamEntry

        pool = UpstreamPool()
        # Inject a mock entry directly (skip real IPC connection).
        mock_client = MagicMock()
        pool._entries['grid'] = _UpstreamEntry('grid', 'ipc-v3://fake', mock_client)

        with caplog.at_level(logging.INFO):
            pool.shutdown()

        assert any('grid' in r.message and 'disconnecting' in r.message.lower()
                    for r in caplog.records)
        mock_client.terminate.assert_called_once()


# -- Tests: c3 relay defaults to native -----------------------------------

class TestRelayDefaultNative:
    """Verify --native is default, --python overrides."""

    def test_default_native_in_help(self):
        from click.testing import CliRunner
        from c_two.cli import cli

        runner = CliRunner()
        result = runner.invoke(cli, ['relay', '--help'])
        assert result.exit_code == 0
        # The flag pair --native/--python should show in help.
        assert '--native' in result.output
        assert '--python' in result.output
