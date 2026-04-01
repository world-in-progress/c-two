"""Transport layer for C-Two.

Provides the complete transport stack for C-Two:
- Server: Rust tokio-based multi-CRM IPC server (via NativeServerBridge)
- SharedClient: thin compatibility wrapper around Rust IPC client
- ICRMProxy: unified proxy (thread-local / IPC / HTTP)
- Registry: cc.register/connect/close/shutdown SOTA API
"""
from __future__ import annotations

__all__ = [
    'SharedClient', 'ICRMProxy',
    'Server', 'CRMSlot',
    'Scheduler', 'ConcurrencyConfig', 'ConcurrencyMode',
    'set_address', 'set_ipc_config', 'register', 'connect', 'close',
    'unregister', 'server_address', 'shutdown', 'serve',
]

_LAZY_IMPORTS: dict[str, tuple[str, str]] = {
    'SharedClient':      ('.client.core',        'SharedClient'),
    'ICRMProxy':         ('.client.proxy',        'ICRMProxy'),
    'Server':          ('.server.native',       'NativeServerBridge'),
    'CRMSlot':           ('.server.native',       'CRMSlot'),
    'Scheduler':         ('.server.scheduler',    'Scheduler'),
    'ConcurrencyConfig': ('.server.scheduler',    'ConcurrencyConfig'),
    'ConcurrencyMode':   ('.server.scheduler',    'ConcurrencyMode'),
    'set_address':       ('.registry',            'set_address'),
    'set_ipc_config':    ('.registry',            'set_ipc_config'),
    'register':          ('.registry',            'register'),
    'connect':           ('.registry',            'connect'),
    'close':             ('.registry',            'close'),
    'unregister':        ('.registry',            'unregister'),
    'server_address':    ('.registry',            'server_address'),
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
