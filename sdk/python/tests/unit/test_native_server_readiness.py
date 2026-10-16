from __future__ import annotations

import pytest

from c_two import _native
from c_two.transport.client.proxy import CRMProxy


def test_runtime_session_exposes_core_host_readiness_api():
    assert hasattr(_native.RuntimeSession, "ensure_host_started")
    assert hasattr(_native.RuntimeSession, "host_started")
    assert not hasattr(_native, "RustServer")
    assert not hasattr(_native, "RustClient")


@pytest.mark.parametrize('owner', [CRMProxy, _native.CoreClient, _native.NativePreparedCall])
def test_client_write_plan_entrypoints_are_retired(owner):
    assert not hasattr(owner, "call_prepared")


@pytest.mark.parametrize('timeout', [0.0, 5.0, float('nan')])
def test_server_start_rejects_removed_timeout(timeout):
    server = _native.RuntimeSession(use_process_relay_anchor=False).ensure_server_bridge()
    try:
        with pytest.raises(TypeError, match='timeout'):
            server.start(timeout=timeout)
        assert not server.is_started()
    finally:
        assert server.shutdown()['completed']
