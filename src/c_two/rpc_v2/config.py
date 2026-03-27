"""Process-level configuration read from environment variables.

Uses ``pydantic-settings`` to load configuration at import time.

Recognised environment variables:

- ``C2_IPC_ADDRESS`` — override the auto-generated IPC server address.
  When set, :func:`cc.register` uses this address instead of generating
  a random ``ipc-v3://cc_auto_…`` path.
- ``C2_IPC_SEGMENT_SIZE`` — buddy pool segment size in bytes
  (default 268 435 456 = 256 MB).  Determines the maximum single-call
  payload that can be transferred via SHM.
- ``C2_IPC_MAX_SEGMENTS`` — maximum number of buddy pool segments
  (default 4, range 1–255).
- ``C2_RELAY_ADDRESS`` — HTTP address of the relay server for service
  discovery.  When set, :func:`cc.register` automatically registers
  the CRM with the relay via ``POST /_register``.

Usage::

    # Shell
    export C2_IPC_ADDRESS=ipc-v3://my_server
    export C2_IPC_SEGMENT_SIZE=2147483648   # 2 GB
    export C2_IPC_MAX_SEGMENTS=8
    export C2_RELAY_ADDRESS=http://127.0.0.1:8080

    # Python
    from c_two.rpc_v2.config import settings
    print(settings.ipc_address)        # 'ipc-v3://my_server'
    print(settings.relay_address)      # 'http://127.0.0.1:8080'
"""
from __future__ import annotations

from pydantic_settings import BaseSettings, SettingsConfigDict


class C2Settings(BaseSettings):
    """C-Two runtime settings sourced from environment variables."""

    model_config = SettingsConfigDict(
        env_prefix='C2_',
    )

    ipc_address: str | None = None
    ipc_segment_size: int | None = None
    ipc_max_segments: int | None = None
    relay_address: str | None = None


settings = C2Settings()
