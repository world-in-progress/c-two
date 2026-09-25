from __future__ import annotations

import math

import pytest

from c_two import _native


def test_runtime_session_exposes_core_host_readiness_api():
    assert hasattr(_native.RuntimeSession, "ensure_host_started")
    assert hasattr(_native.RuntimeSession, "host_started")
    assert not hasattr(_native, "RustServer")


def test_core_client_exposes_prepared_payload_call_api():
    assert hasattr(_native.CoreClient, "call_prepared")
    assert not hasattr(_native, "RustClient")


def test_server_start_rejects_invalid_timeout():
    from c_two.transport import Server

    server = Server(bind_address="ipc://unit_bad_timeout_native")
    try:
        with pytest.raises(ValueError, match="timeout"):
            server.start(math.nan)
    finally:
        try:
            server.shutdown()
        except Exception:
            pass
