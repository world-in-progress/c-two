"""Transport layer for C-Two.

Provides the complete transport stack for C-Two:
- Server: Rust tokio-based multi-CRM IPC server (via NativeServerBridge)
- CRMProxy: unified proxy (thread-local / IPC / HTTP)
- Registry: cc.register/connect/close/shutdown SOTA API
"""
from __future__ import annotations

__all__ = [
    'CRMProxy',
    'Server', 'CRMSlot',
    'Scheduler', 'ConcurrencyConfig', 'ConcurrencyMode',
    'with_call_options',
    'set_call_execution_limits',
    'call_execution_snapshot',
    'set_transport_policy', 'set_server', 'set_client',
    'set_relay_anchor',
    'register', 'connect', 'close',
    'unregister', 'server_address', 'server_id', 'shutdown', 'serve',
]

_LAZY_IMPORTS: dict[str, tuple[str, str]] = {
    'with_call_options': ('.client.proxy', 'with_call_options'),
    'set_call_execution_limits': ('.registry', 'set_call_execution_limits'),
    'call_execution_snapshot': ('.registry', 'call_execution_snapshot'),
    'CRMProxy':         ('.client.proxy',        'CRMProxy'),
    'Server':          ('.server.native',       'NativeServerBridge'),
    'CRMSlot':           ('.server.native',       'CRMSlot'),
    'Scheduler':         ('.server.scheduler',    'Scheduler'),
    'ConcurrencyConfig': ('.server.scheduler',    'ConcurrencyConfig'),
    'ConcurrencyMode':   ('.server.scheduler',    'ConcurrencyMode'),
    'set_transport_policy': ('.registry',         'set_transport_policy'),
    'set_server':        ('.registry',            'set_server'),
    'set_client':        ('.registry',            'set_client'),
    'set_relay_anchor':         ('.registry',            'set_relay_anchor'),
    'register':          ('.registry',            'register'),
    'connect':           ('.registry',            'connect'),
    'close':             ('.registry',            'close'),
    'unregister':        ('.registry',            'unregister'),
    'server_address':    ('.registry',            'server_address'),
    'server_id':         ('.registry',            'server_id'),
    'shutdown':          ('.registry',            'shutdown'),
    'serve':             ('.registry',            'serve'),
}


def __getattr__(name: str):
    if name in _LAZY_IMPORTS:
        mod_path, attr = _LAZY_IMPORTS[name]
        import importlib
        mod = importlib.import_module(mod_path, __name__)
        val = getattr(mod, attr)
        globals()[name] = val
        return val
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
