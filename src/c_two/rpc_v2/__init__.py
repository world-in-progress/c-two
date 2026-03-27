"""RPC v2 — shared-client, control-plane routed RPC module.

This module provides:
- **SharedClient**: concurrent multiplexed IPC v3 client shared across
  multiple ICRM consumers (N × 256 MB → 1 × 256 MB).
- **ClientPool**: reference-counted client management with grace-period
  destruction.
- **HttpClient** / **HttpClientPool**: concurrent HTTP client and pool
  for cross-node CRM access through a relay server.
- **RelayV2**: Python HTTP relay server bridging HTTP → IPC v3.
- **ServerV2**: asyncio-based IPC v3 server with wire v2 control-plane
  routing (handshake v5, method index exchange, pure SHM payload).
- **Scheduler**: read/write-aware task scheduler for CRM method execution.
- **ICRMProxy**: unified client-compatible proxy (thread-local / IPC / HTTP).
"""
from __future__ import annotations

from .client import SharedClient
from .http_client import HttpClient, HttpClientPool
from .pool import ClientPool
from .proxy import ICRMProxy
from .registry import (
    set_address,
    set_ipc_config,
    register,
    connect,
    close,
    unregister,
    server_address,
    shutdown,
)
from .relay import RelayV2, UpstreamPool
from .scheduler import Scheduler, ConcurrencyConfig, ConcurrencyMode
from .server import ServerV2, CRMSlot

__all__ = [
    'SharedClient', 'ClientPool',
    'HttpClient', 'HttpClientPool',
    'RelayV2', 'UpstreamPool',
    'ServerV2', 'CRMSlot',
    'ICRMProxy',
    'Scheduler', 'ConcurrencyConfig', 'ConcurrencyMode',
    'set_address', 'set_ipc_config', 'register', 'connect', 'close', 'unregister',
    'server_address', 'shutdown',
]
