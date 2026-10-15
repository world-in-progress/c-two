from __future__ import annotations

import pytest

from c_two import _native


def test_runtime_session_exposes_core_host_readiness_api():
    assert hasattr(_native.RuntimeSession, "ensure_host_started")
    assert hasattr(_native.RuntimeSession, "host_started")
    assert not hasattr(_native, "RustServer")


def test_core_client_exposes_prepared_payload_call_api():
    assert hasattr(_native.CoreClient, "call_prepared")
    assert not hasattr(_native, "RustClient")


@pytest.mark.parametrize('timeout', [0.0, 5.0, float('nan')])
def test_server_start_rejects_removed_timeout(timeout):
    server = _native.RuntimeSession(use_process_relay_anchor=False).ensure_server_bridge()
    try:
        with pytest.raises(TypeError, match='timeout'):
            server.start(timeout=timeout)
        assert not server.is_started()
    finally:
        assert server.shutdown()['completed']
