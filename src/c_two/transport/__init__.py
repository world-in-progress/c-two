"""Transport layer — promoted from rpc_v2/.

Provides the complete transport stack for C-Two:
- ServerV2: asyncio-based multi-CRM IPC server
- SharedClient: concurrent multiplexed IPC client
- ClientPool: reference-counted client lifecycle
- ICRMProxy: unified proxy (thread-local / IPC / HTTP)
- RelayV2: HTTP → IPC bridge (Starlette + uvicorn)
- Registry: cc.register/connect/close/shutdown SOTA API
"""
from __future__ import annotations

__all__ = [
    'SharedClient', 'ClientPool', 'ICRMProxy',
    'HttpClient', 'HttpClientPool',
    'ServerV2', 'CRMSlot',
    'Scheduler', 'ConcurrencyConfig', 'ConcurrencyMode',
    'RelayV2', 'UpstreamPool',
    'set_address', 'set_ipc_config', 'register', 'connect', 'close',
    'unregister', 'server_address', 'shutdown', 'serve',
]

_LAZY_IMPORTS: dict[str, tuple[str, str]] = {
    'SharedClient':      ('.client.core',        'SharedClient'),
    'ClientPool':        ('.client.pool',         'ClientPool'),
    'ICRMProxy':         ('.client.proxy',        'ICRMProxy'),
    'HttpClient':        ('.client.http',         'HttpClient'),
    'HttpClientPool':    ('.client.http',         'HttpClientPool'),
    'ServerV2':          ('.server.core',         'ServerV2'),
    'CRMSlot':           ('.server.core',         'CRMSlot'),
    'Scheduler':         ('.server.scheduler',    'Scheduler'),
    'ConcurrencyConfig': ('.server.scheduler',    'ConcurrencyConfig'),
    'ConcurrencyMode':   ('.server.scheduler',    'ConcurrencyMode'),
    'RelayV2':           ('.relay.core',          'RelayV2'),
    'UpstreamPool':      ('.relay.core',          'UpstreamPool'),
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
