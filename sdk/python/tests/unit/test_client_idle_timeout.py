"""Client cache timeout projection through the public IPC configuration path."""
from typing import get_type_hints

import c_two as cc
from c_two import _native
from c_two.config import ClientIPCOverrides


def test_client_idle_timeout_projection(monkeypatch):
    monkeypatch.delenv('C2_IPC_CLIENT_IDLE_TIMEOUT', raising=False)
    assert get_type_hints(ClientIPCOverrides)['client_idle_timeout'] is float
    assert _native.resolve_client_ipc_config(None, None)['client_idle_timeout'] == 60.0
    monkeypatch.setenv('C2_IPC_CLIENT_IDLE_TIMEOUT', '0.125')
    runtime = cc.Runtime()
    session = runtime._runtime_session  # noqa: SLF001
    try:
        assert session.client_ipc_config['client_idle_timeout'] == 0.125
        runtime.set_client(ipc_overrides={'client_idle_timeout': 0.25})
        assert session.client_ipc_overrides['client_idle_timeout'] == 0.25
        assert session.client_ipc_config['client_idle_timeout'] == 0.25
        runtime.set_client(ipc_overrides={'client_idle_timeout': 0.0})
        assert session.client_ipc_config['client_idle_timeout'] == 0.0
    finally:
        assert runtime.shutdown()['completed']
