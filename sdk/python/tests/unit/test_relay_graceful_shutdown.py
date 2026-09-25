"""Tests for graceful relay shutdown handling.

Verifies that CRM processes do not crash when the relay is unreachable
during ``cc.unregister()`` or ``cc.shutdown()``.
"""
from __future__ import annotations

import pytest

import c_two as cc
from c_two.config import settings
from c_two.transport.registry import _ProcessRegistry


# -- Helpers ---------------------------------------------------------------

@cc.crm(namespace='cc.test.relay_shutdown', version='0.1.0')
class IRelayShutdownCRM:
    def ping(self) -> str:
        ...

    @cc.on_shutdown
    def cleanup(self) -> None:
        ...


class RelayShutdownCRM:
    def __init__(self) -> None:
        self.cleanup_calls = 0

    def ping(self) -> str:
        return 'pong'

    def cleanup(self) -> None:
        self.cleanup_calls += 1


# -- Tests: unregister tolerates relay absence -----------------------------

class TestUnregisterRelayAbsence:
    """Explicit unregister reports relay failures; shutdown stays best-effort."""

    def setup_method(self):
        self.registry = _ProcessRegistry()
        settings.relay_anchor_address = None

    def teardown_method(self):
        try:
            self.registry.shutdown()
        except Exception:
            pass
        settings.relay_anchor_address = None

    def test_unregister_no_relay(self):
        """Unregister works when relay calls are no-ops."""
        self.registry.register(IRelayShutdownCRM, RelayShutdownCRM(), name='test')
        self.registry.unregister('test')  # Should not raise

    def test_late_relay_anchor_change_is_rejected_before_unregister(self):
        """A started Core host keeps the relay projection it registered with."""
        impl = RelayShutdownCRM()
        self.registry.register(IRelayShutdownCRM, impl, name='test_down')

        with pytest.raises(RuntimeError, match='relay anchor is frozen'):
            self.registry.set_relay_anchor('http://127.0.0.1:9')

        self.registry.unregister('test_down')
        assert 'test_down' not in self.registry.names
        assert impl.cleanup_calls == 1

        self.registry.shutdown()
        assert impl.cleanup_calls == 1

    def test_shutdown_ignores_late_process_relay_setting(self):
        """Shutdown uses the Core host's frozen relay projection."""
        impl = RelayShutdownCRM()
        self.registry.register(IRelayShutdownCRM, impl, name='test_sd')
        settings.relay_anchor_address = 'http://127.0.0.1:9'

        self.registry.shutdown()

        assert impl.cleanup_calls == 1

    def test_shutdown_after_unregister_does_not_double_call_callback(self):
        """Native removed-route outcomes drive callback invocation once."""
        impl = RelayShutdownCRM()
        self.registry.register(IRelayShutdownCRM, impl, name='test_once')
        self.registry.unregister('test_once')

        self.registry.shutdown()

        assert impl.cleanup_calls == 1

    def test_reregister_after_unregister_still_works(self):
        first = RelayShutdownCRM()
        second = RelayShutdownCRM()
        self.registry.register(IRelayShutdownCRM, first, name='test_reuse')
        self.registry.unregister('test_reuse')

        self.registry.register(IRelayShutdownCRM, second, name='test_reuse')
        assert self.registry.names == ['test_reuse']
        self.registry.unregister('test_reuse')

        assert first.cleanup_calls == 1
        assert second.cleanup_calls == 1

    def test_register_surfaces_relay_http_error_from_native_session(self):
        self.registry.set_relay_anchor('http://127.0.0.1:9')

        with pytest.raises(RuntimeError, match='relay_prepare'):
            self.registry.register(IRelayShutdownCRM, RelayShutdownCRM(), name='test_relay_fail')

        assert 'test_relay_fail' not in self.registry.names
        assert self.registry.get_server_id() is None
        assert self.registry.get_server_address() is None
