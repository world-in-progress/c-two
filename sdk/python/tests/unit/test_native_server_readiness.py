from __future__ import annotations

import math

import pytest

from c_two import _native


def test_rust_server_exposes_native_readiness_api():
    assert hasattr(_native.RustServer, "start_and_wait")
    assert hasattr(_native.RustServer, "is_ready")
    assert hasattr(_native.RustServer, "is_running")


def test_rust_client_exposes_prepared_payload_call_api():
    assert hasattr(_native.RustClient, "call_prepared")


def test_start_and_wait_rejects_invalid_timeout(monkeypatch):
    # Construct through the public Python Server wrapper in implementation tests;
    # this low-level test only locks API validation shape after PyO3 exposes it.
    from c_two.transport import Server

    server = Server(bind_address="ipc://unit_bad_timeout_native")
    try:
        with pytest.raises((RuntimeError, ValueError), match="timeout"):
            server._rust_server.start_and_wait(math.nan)  # noqa: SLF001
    finally:
        try:
            server.shutdown()
        except Exception:
            pass
